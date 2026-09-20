//! End-to-end test: publish and clone through an S3-backed wheat-server.
//!
//! Requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-packed-segment-bench).
//! Run with:
//!   cargo test -p wheat-server --test s3_packed_test -- --ignored --nocapture

use std::collections::HashSet;
use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{
    ChangeProposal, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    ObjectId, Repository, RevisionEdits, Tree,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BatchObjectStore, ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    PersistedRepositoryMutator, PublicationStore, RemotePackedStore, RemotePackedStoreConfig,
    RepositoryHead, RepositoryPublisher, RepositoryReader, S3ObjectStoreConfig,
    finalize_mutation_publication, prepare_mutation_publication,
};

// ---------------------------------------------------------------------------
// Helpers (same pattern as clone_fetch_publish_test.rs)
// ---------------------------------------------------------------------------

fn count_files_recursive(dir: &std::path::Path) -> usize {
    let mut count = 0;
    if !dir.exists() {
        return 0;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&path) {
            for entry in entries.flatten() {
                let ft = entry.file_type().unwrap();
                if ft.is_file() {
                    count += 1;
                } else if ft.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }
    count
}

fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

/// Create a repository with `num_changes` changes, each published in two
/// steps (like `wheat change new` + `wheat add`): first an empty change,
/// then a revision with file edits. This produces the same object count
/// as the CLI — every change gets two generations of incremental state.
async fn create_repo_incremental(
    objects: &LooseObjectStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    num_changes: usize,
) -> RepositoryHead {
    // Initialize genesis.
    let repo = Repository::new(Tree::new());
    let publisher = RepositoryPublisher::new(objects, content, heads);
    publisher.initialize(&repo).await.unwrap();

    let batch = BatchObjectStore::new(objects, 1024, 8 * 1024 * 1024);
    let mutator = PersistedRepositoryMutator::new(&batch, content, heads);

    for i in 0..num_changes {
        let file_data = format!(
            "file content {i} with some bulk data to make it realistic: {}",
            "x".repeat(100)
        );
        content.put(file_data.as_bytes()).unwrap();

        // Step 1: Create an empty change (like `wheat change new --message "..."`)
        let head = heads.read().unwrap();
        let reader = RepositoryReader::new(objects, content);
        let envelope = reader.envelope(head.envelope).await.unwrap();
        let state = reader.state(envelope.state).await.unwrap();

        let empty_proposal = ChangeProposal {
            id: wheat_core::ChangeId::new(),
            metadata: wheat_core::ChangeMetadata {
                title: format!("change {i}"),
                description: String::new(),
                created_at: wheat_core::Timestamp {
                    seconds: 1_000_000 + i as i64,
                    offset_minutes: 0,
                },
                creator: wheat_core::Actor {
                    name: "bench".into(),
                    email: "bench@test".into(),
                },
                provenance: None,
            },
            dependencies: std::collections::BTreeSet::new(),
            revision_metadata: wheat_core::RevisionMetadata {
                message: format!("change {i}"),
                authored_at: wheat_core::Timestamp {
                    seconds: 1_000_000 + i as i64,
                    offset_minutes: 0,
                },
                committed_at: wheat_core::Timestamp {
                    seconds: 1_000_000 + i as i64,
                    offset_minutes: 0,
                },
                author: wheat_core::Actor {
                    name: "bench".into(),
                    email: "bench@test".into(),
                },
                committer: wheat_core::Actor {
                    name: "bench".into(),
                    email: "bench@test".into(),
                },
                provenance: None,
            },
            edits: RevisionEdits::default(), // empty — no file edits yet
        };
        let change_id = empty_proposal.id;
        let (prepared, outcome) = mutator
            .create_on_frontier(head, state.base_view, state.base_view, empty_proposal)
            .await
            .unwrap();
        let resulting_view = prepared.resulting_view();
        let publication = prepare_mutation_publication(&batch, content, heads, head, prepared)
            .await
            .unwrap();
        batch.flush().await.unwrap();
        finalize_mutation_publication(heads, publication).unwrap();

        // Step 2: Add the file edit as a new revision (like `wheat add`)
        let head2 = heads.read().unwrap();
        let reader2 = RepositoryReader::new(objects, content);
        let envelope2 = reader2.envelope(head2.envelope).await.unwrap();
        let state2 = reader2.state(envelope2.state).await.unwrap();

        let revision_metadata = wheat_core::RevisionMetadata {
            message: format!("add file {i}"),
            authored_at: wheat_core::Timestamp {
                seconds: 1_000_000 + i as i64,
                offset_minutes: 0,
            },
            committed_at: wheat_core::Timestamp {
                seconds: 1_000_000 + i as i64,
                offset_minutes: 0,
            },
            author: wheat_core::Actor {
                name: "bench".into(),
                email: "bench@test".into(),
            },
            committer: wheat_core::Actor {
                name: "bench".into(),
                email: "bench@test".into(),
            },
            provenance: None,
        };
        let file_edits = edit(&format!("src/file-{i}.txt"), file_data.as_bytes());
        let (prepared2, _outcome2) = mutator
            .revise(
                head2,
                state2.base_view,
                resulting_view,
                change_id,
                outcome.revision_id,
                revision_metadata,
                file_edits,
            )
            .await
            .unwrap();
        let publication2 = prepare_mutation_publication(&batch, content, heads, head2, prepared2)
            .await
            .unwrap();
        batch.flush().await.unwrap();
        finalize_mutation_publication(heads, publication2).unwrap();
    }

    heads.read().unwrap()
}

fn local_stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

fn s3_config(prefix_suffix: &str) -> Option<RemotePackedStoreConfig> {
    let endpoint = std::env::var("WHEAT_TEST_S3_ENDPOINT").ok()?;
    let bucket = std::env::var("WHEAT_TEST_S3_BUCKET").ok()?;
    let region = std::env::var("WHEAT_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into());
    let access_key_id = std::env::var("WHEAT_TEST_S3_ACCESS_KEY_ID").ok()?;
    let secret_access_key = std::env::var("WHEAT_TEST_S3_SECRET_ACCESS_KEY").ok()?;
    Some(RemotePackedStoreConfig {
        s3: S3ObjectStoreConfig {
            endpoint: endpoint.clone(),
            bucket,
            region,
            access_key_id,
            secret_access_key,
            prefix: format!(
                "wheat-s3-packed-test-{}-{}",
                std::process::id(),
                prefix_suffix
            ),
            allow_http: endpoint.starts_with("http://"),
        },
        segment_cache_bytes: Some(64 * 1024 * 1024),
    })
}

async fn start_s3_server(
    config: &RemotePackedStoreConfig,
    head_dir: &std::path::Path,
    repo_name: &str,
) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let store = RemotePackedStore::open(config).await.unwrap();
    let content = store.content_store();
    let heads = LocalPublicationStore::new(head_dir.join("repository.head"));
    let registry =
        Registry::single_with_stores(repo_name.to_string(), Box::new(store), content, heads);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();

    tokio::spawn(async move {
        run_until(addr, registry, ServerConfig::new(), ready_tx, shutdown_rx)
            .await
            .unwrap();
    });

    let bound = ready_rx.await.unwrap();
    (bound, shutdown_tx)
}

async fn send_msg(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, msg: &WheatProtocolMessage) {
    let bytes = encode_message(msg).unwrap();
    ws.send(Message::Binary(bytes.into())).await.unwrap();
}

async fn recv_msg(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>) -> WheatProtocolMessage {
    loop {
        let msg = ws.next().await.unwrap().unwrap();
        match msg {
            Message::Binary(bytes) => return decode_message(&bytes).unwrap(),
            Message::Ping(data) => {
                ws.send(Message::Pong(data)).await.unwrap();
            }
            Message::Pong(_) | Message::Frame(_) => {}
            other => panic!("unexpected message: {other:?}"),
        }
    }
}

async fn connect_and_handshake(addr: SocketAddr) -> WebSocketStream<MaybeTlsStream<TcpStream>> {
    let url = format!("ws://{addr}");
    let (mut ws, _) = connect_async(&url).await.unwrap();

    let server_hello = recv_msg(&mut ws).await;
    assert!(matches!(server_hello, WheatProtocolMessage::ServerHello(_)));

    send_msg(
        &mut ws,
        &WheatProtocolMessage::ClientHello(ClientHelloOutput {
            message_type: 0x02,
            selected_version: 1,
            auth_token: vec![],
            receive_window_bytes: 16 * 1024 * 1024,
            receive_window_messages: 256,
        }),
    )
    .await;

    let established = recv_msg(&mut ws).await;
    assert!(matches!(
        established,
        WheatProtocolMessage::SessionEstablished(_)
    ));

    ws
}

async fn open_repo(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    name: &str,
) -> (u64, Digest256) {
    send_msg(
        ws,
        &WheatProtocolMessage::RepositoryOpen(RepositoryOpenOutput {
            message_type: 0x10,
            name: name.to_string(),
        }),
    )
    .await;

    let msg = recv_msg(ws).await;
    match msg {
        WheatProtocolMessage::RepositoryState(rs) => (rs.head_generation, rs.head_envelope),
        other => panic!("expected RepositoryState, got {other:?}"),
    }
}

/// Client-side iterative root-first fetch.
async fn client_fetch_objects(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    op_id: u32,
    objects: &LooseObjectStore,
    content: &ContentStore,
    envelope_id: wheat_core::RepositoryEnvelopeId,
) -> usize {
    send_msg(
        ws,
        &WheatProtocolMessage::FetchRequest(FetchRequestOutput {
            message_type: 0x20,
            op_id,
            labels: vec![],
            since_generation: 0,
            since_envelope: Digest256 { bytes: vec![0; 32] },
        }),
    )
    .await;

    let mut total = 0;
    let mut want_list: Vec<ObjectId> = Vec::new();
    let mut known = HashSet::new();

    loop {
        let msg = recv_msg(ws).await;
        match msg {
            WheatProtocolMessage::ObjectBatch(batch) => {
                assert_eq!(batch.op_id, op_id);

                for entry in &batch.objects {
                    let claimed_id =
                        ObjectId::from_digest(entry.id.bytes.as_slice().try_into().unwrap());
                    let actual_id = ObjectId::of_canonical_bytes(&entry.data);
                    assert_eq!(actual_id, claimed_id, "object identity mismatch");

                    objects
                        .put_if_absent(claimed_id, &entry.data)
                        .await
                        .unwrap();
                    known.insert(claimed_id);
                    total += 1;

                    if let Ok(children) = wheat_core::extract_object_references(&entry.data) {
                        for child_id in children {
                            if !known.contains(&child_id) {
                                match objects.get(child_id).await {
                                    Ok(_) => {
                                        known.insert(child_id);
                                    }
                                    Err(_) => {
                                        want_list.push(child_id);
                                    }
                                }
                            }
                        }
                    }
                }

                want_list.retain(|id| !known.contains(id));
                let mut seen = HashSet::new();
                want_list.retain(|id| seen.insert(*id));

                // Walk retained sets until convergence.
                if want_list.is_empty() {
                    loop {
                        let reader = RepositoryReader::new(objects, content);
                        let env = reader.envelope(envelope_id).await.unwrap();
                        let state = reader.state(env.state).await.unwrap();

                        let retained_store = wheat_store::RetainedSetStore::new(objects);
                        let mut bfs_frontier: Vec<ObjectId> = Vec::new();
                        let mut found_new = false;
                        for root in [
                            state.retained_views,
                            state.retained_revisions,
                            state.retained_applications,
                            state.retained_changes,
                        ] {
                            let mut cursor = retained_store.cursor(root).await.unwrap();
                            while let Some(oid) = cursor.next().await.unwrap() {
                                if !known.contains(&oid) {
                                    found_new = true;
                                    match objects.get(oid).await {
                                        Ok(obj) => {
                                            known.insert(oid);
                                            if let Ok(children) =
                                                wheat_core::extract_object_references(
                                                    &obj.canonical_bytes,
                                                )
                                            {
                                                for c in children {
                                                    if !known.contains(&c) {
                                                        bfs_frontier.push(c);
                                                    }
                                                }
                                            }
                                        }
                                        Err(_) => {
                                            want_list.push(oid);
                                        }
                                    }
                                }
                            }
                        }
                        while let Some(oid) = bfs_frontier.pop() {
                            if known.contains(&oid) {
                                continue;
                            }
                            match objects.get(oid).await {
                                Ok(obj) => {
                                    known.insert(oid);
                                    if let Ok(children) =
                                        wheat_core::extract_object_references(&obj.canonical_bytes)
                                    {
                                        for c in children {
                                            if !known.contains(&c) {
                                                bfs_frontier.push(c);
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    want_list.push(oid);
                                }
                            }
                        }
                        want_list.retain(|id| !known.contains(id));
                        let mut seen2 = HashSet::new();
                        want_list.retain(|id| seen2.insert(*id));

                        if !found_new && want_list.is_empty() {
                            break;
                        }
                        if !want_list.is_empty() {
                            break;
                        }
                    }
                }

                if want_list.is_empty() {
                    send_msg(
                        ws,
                        &WheatProtocolMessage::FetchComplete(FetchCompleteOutput {
                            message_type: 0x23,
                            op_id,
                        }),
                    )
                    .await;
                    break;
                }

                let batch_size = want_list.len().min(MAX_WANT_IDS);
                let want_batch: Vec<Digest256> = want_list
                    .drain(..batch_size)
                    .map(|id| Digest256 {
                        bytes: id.as_bytes().to_vec(),
                    })
                    .collect();

                send_msg(
                    ws,
                    &WheatProtocolMessage::ObjectWant(ObjectWantOutput {
                        message_type: 0x22,
                        op_id,
                        ids: want_batch,
                    }),
                )
                .await;
            }
            other => panic!("unexpected message during fetch: {other:?}"),
        }
    }

    total
}

/// Client-side content fetch.
async fn client_fetch_content(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    op_id: u32,
    objects: &LooseObjectStore,
    content: &ContentStore,
    envelope_id: &Digest256,
) -> usize {
    let env_id = wheat_core::RepositoryEnvelopeId::from_digest(
        envelope_id.bytes.as_slice().try_into().unwrap(),
    );
    let reader = RepositoryReader::new(objects, content);
    let envelope = reader.envelope(env_id).await.unwrap();
    let state = reader.state(envelope.state).await.unwrap();
    let manifest = reader.manifest(state.required_content).await.unwrap();

    let content_set = wheat_store::ContentSetStore::new(objects);
    let mut cursor = content_set.cursor(manifest.content).await.unwrap();
    let mut missing = Vec::new();
    while let Some(content_id) = cursor.next().await.unwrap() {
        if !content.contains(content_id) {
            missing.push(Digest256 {
                bytes: content_id.as_bytes().to_vec(),
            });
        }
    }

    if missing.is_empty() {
        return 0;
    }

    let total = missing.len();

    send_msg(
        ws,
        &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
            message_type: 0x30,
            op_id,
            content_ids: missing,
        }),
    )
    .await;

    let mut received = 0;
    while received < total {
        let msg = recv_msg(ws).await;
        match msg {
            WheatProtocolMessage::ContentBatch(cb) => {
                assert_eq!(cb.op_id, op_id);
                for entry in &cb.contents {
                    let id = ContentId::from_digest(entry.id.bytes.as_slice().try_into().unwrap());
                    let actual = ContentId::of(&entry.data);
                    assert_eq!(actual, id, "content identity mismatch");
                    content.put(&entry.data).unwrap();
                    received += 1;
                }
            }
            WheatProtocolMessage::ContentChunk(chunk) => {
                assert_eq!(chunk.op_id, op_id);
                if chunk.is_final {
                    let id = ContentId::from_digest(
                        chunk.content_id.bytes.as_slice().try_into().unwrap(),
                    );
                    let actual = ContentId::of(&chunk.data);
                    assert_eq!(actual, id, "chunked content identity mismatch");
                    content.put(&chunk.data).unwrap();
                    received += 1;
                }
            }
            _ => {}
        }
    }

    total
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Publish a multi-change repository to an S3-backed wheat-server,
/// then clone it back and verify completeness.
///
/// This is the full end-to-end flow: objects go to S3 via packed segments,
/// content goes to S3 as individual keys, and the clone reads everything
/// back from S3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-packed-segment-bench)"]
async fn publish_clone_through_s3() {
    let Some(config) = s3_config("publish-clone") else {
        eprintln!("skipping: WHEAT_TEST_S3_* not configured");
        return;
    };

    // --- Step 1: Create a local repository with several changes ---
    // Each change is individually published (like `wheat change new` does),
    // so the object count matches a real repository's incremental growth.
    let local_dir = tempfile::tempdir().unwrap();
    let local_path = local_dir.path().join("origin");
    std::fs::create_dir(&local_path).unwrap();

    let (local_objects, local_content, local_heads) = local_stores(&local_path);

    let num_changes = 10;
    let local_head =
        create_repo_incremental(&local_objects, &local_content, &local_heads, num_changes).await;

    eprintln!(
        "local repo: generation={}, {} changes, {} objects",
        local_head.generation,
        num_changes,
        count_files_recursive(&local_path.join("objects"))
    );

    // --- Step 2: Initialize genesis on the S3-backed server ---
    // We need a valid genesis state on S3 for the server to read at
    // RepositoryOpen. We do this by creating an empty repo directly
    // on the RemotePackedStore.
    let s3_dir = tempfile::tempdir().unwrap();
    let s3_head_dir = s3_dir.path().join("server");
    std::fs::create_dir(&s3_head_dir).unwrap();

    {
        let s3_store = RemotePackedStore::open(&config).await.unwrap();
        let s3_content = s3_store.content_store();
        let s3_heads = LocalPublicationStore::new(s3_head_dir.join("repository.head"));

        // Create an empty genesis repo on S3.
        let genesis_repo = Repository::new(Tree::new());
        let s3_publisher = RepositoryPublisher::new(&s3_store, &s3_content, &s3_heads);
        s3_publisher.initialize(&genesis_repo).await.unwrap();

        // Seal the genesis objects into a segment on S3.
        s3_store.seal().await.unwrap();

        let genesis_head = s3_heads.read().unwrap();
        eprintln!(
            "S3 genesis: generation={}, envelope={}",
            genesis_head.generation, genesis_head.envelope
        );
    }

    // --- Step 3: Start S3-backed server and publish ---
    let (addr, shutdown) = start_s3_server(&config, &s3_head_dir, "bench/repo").await;
    eprintln!("S3-backed server listening on {addr}");

    let mut ws = connect_and_handshake(addr).await;
    let (server_gen, server_env) = open_repo(&mut ws, "bench/repo").await;
    eprintln!("server state: generation={server_gen}");

    // Publish local repo to S3 server.
    let local_envelope = local_head.envelope;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishRequest(PublishRequestOutput {
            message_type: 0x40,
            op_id: 1,
            publish_key: Uuid128 { bytes: vec![1; 16] },
            envelope_id: Digest256 {
                bytes: local_envelope.as_bytes().to_vec(),
            },
            expected_generation: server_gen,
            expected_envelope: server_env.clone(),
            force: false,
        }),
    )
    .await;

    // Drive the publish protocol: ObjectsNeeded/ObjectBatch loop.
    let mut objects_sent = 0;
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::ObjectsNeeded(on) => {
                assert_eq!(on.op_id, 1);
                let mut batch_objects = Vec::new();
                for digest in &on.ids {
                    let oid = ObjectId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                    let obj = local_objects.get(oid).await.unwrap();
                    batch_objects.push(ObjectEntry {
                        id: digest.clone(),
                        data: obj.canonical_bytes.to_vec(),
                    });
                    objects_sent += 1;
                }
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                        message_type: 0x21,
                        op_id: 1,
                        sequence: 0,
                        objects: batch_objects,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ObjectsComplete(oc) => {
                assert_eq!(oc.op_id, 1);
                break;
            }
            other => panic!("unexpected during publish objects: {other:?}"),
        }
    }
    eprintln!("published {objects_sent} objects to S3 server");

    // Content phase.
    let mut content_sent = 0;
    let msg = recv_msg(&mut ws).await;
    match msg {
        WheatProtocolMessage::ContentNeeded(cn) => {
            assert_eq!(cn.op_id, 1);
            for digest in &cn.content_ids {
                let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                let data = local_content.get(id).unwrap();
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                        message_type: 0x44,
                        op_id: 1,
                        content_id: digest.clone(),
                        data,
                    }),
                )
                .await;
                content_sent += 1;
            }
            send_msg(
                &mut ws,
                &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                    message_type: 0x46,
                    op_id: 1,
                }),
            )
            .await;
        }
        other => panic!("expected ContentNeeded, got {other:?}"),
    }
    eprintln!("published {content_sent} content blobs to S3 server");

    // Commit.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id: 1,
        }),
    )
    .await;

    // Wait for result.
    let published_generation;
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(pr) => {
                assert_eq!(pr.op_id, 1);
                assert!(
                    matches!(pr.status, PublishResultStatus::PublishStatusOk(_)),
                    "publish should succeed, got: {:?}, message: {}",
                    pr.status,
                    pr.message,
                );
                published_generation = pr.new_generation;
                eprintln!("publish succeeded: generation={}", published_generation);
                break;
            }
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }

    ws.close(None).await.ok();

    // --- Step 4: Clone from the S3-backed server ---
    // Reconnect and clone. This exercises the full S3 read path: the server
    // reads objects from packed segments and content from individual S3 keys.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();
    let (clone_objects, clone_content, clone_heads) = local_stores(&clone_path);

    let mut ws = connect_and_handshake(addr).await;
    let (clone_gen, clone_env) = open_repo(&mut ws, "bench/repo").await;
    assert_eq!(
        clone_gen, published_generation,
        "server should be at published generation"
    );

    let env_id = wheat_core::RepositoryEnvelopeId::from_digest(
        clone_env.bytes.as_slice().try_into().unwrap(),
    );

    let clone_start = std::time::Instant::now();

    let obj_count = client_fetch_objects(&mut ws, 2, &clone_objects, &clone_content, env_id).await;
    let content_count =
        client_fetch_content(&mut ws, 3, &clone_objects, &clone_content, &clone_env).await;

    let clone_elapsed = clone_start.elapsed();

    let head = RepositoryHead {
        generation: clone_gen,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    eprintln!(
        "cloned from S3: {} objects, {} content blobs in {:?}",
        obj_count, content_count, clone_elapsed
    );

    // Validate the clone is complete.
    let reader = RepositoryReader::new(&clone_objects, &clone_content);
    let env = reader.envelope(env_id).await.unwrap();
    reader
        .validate_complete(env.state)
        .await
        .expect("cloned state from S3 should be complete");

    // Verify all content is present.
    for i in 0..num_changes {
        let file_data = format!(
            "file content {i} with some bulk data to make it realistic: {}",
            "x".repeat(100)
        );
        let id = ContentId::of(file_data.as_bytes());
        let retrieved = clone_content.get(id).unwrap();
        assert_eq!(
            retrieved,
            file_data.as_bytes(),
            "content mismatch for file {i}"
        );
    }

    eprintln!("publish_clone_through_s3: PASSED ({obj_count} objects, {content_count} content)");

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}

/// Benchmark-oriented test: publish a larger repository (configurable via
/// WHEAT_BENCH_CHANGES env var, default 50) to an S3-backed server, then
/// clone it and report timing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-packed-segment-bench)"]
async fn publish_clone_s3_benchmark() {
    let Some(config) = s3_config("benchmark") else {
        eprintln!("skipping: WHEAT_TEST_S3_* not configured");
        return;
    };

    let num_changes: usize = std::env::var("WHEAT_BENCH_CHANGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    // --- Create local repo with many changes (incrementally, like the CLI) ---
    let local_dir = tempfile::tempdir().unwrap();
    let local_path = local_dir.path().join("origin");
    std::fs::create_dir(&local_path).unwrap();

    let (local_objects, local_content, local_heads) = local_stores(&local_path);
    let local_head =
        create_repo_incremental(&local_objects, &local_content, &local_heads, num_changes).await;

    // Count objects by walking the loose store directory.
    let obj_count_local = count_files_recursive(&local_path.join("objects"));

    eprintln!("=== S3 Benchmark: {num_changes} changes, {obj_count_local} objects ===");

    // --- Initialize genesis on S3 ---
    let s3_dir = tempfile::tempdir().unwrap();
    let s3_head_dir = s3_dir.path().join("server");
    std::fs::create_dir(&s3_head_dir).unwrap();

    {
        let s3_store = RemotePackedStore::open(&config).await.unwrap();
        let s3_content = s3_store.content_store();
        let s3_heads = LocalPublicationStore::new(s3_head_dir.join("repository.head"));

        let genesis_repo = Repository::new(Tree::new());
        let s3_publisher = RepositoryPublisher::new(&s3_store, &s3_content, &s3_heads);
        s3_publisher.initialize(&genesis_repo).await.unwrap();
        s3_store.seal().await.unwrap();
    }

    // --- Start S3 server ---
    let (addr, shutdown) = start_s3_server(&config, &s3_head_dir, "bench/repo").await;

    // --- Publish ---
    let publish_start = std::time::Instant::now();

    let mut ws = connect_and_handshake(addr).await;
    let (server_gen, server_env) = open_repo(&mut ws, "bench/repo").await;

    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishRequest(PublishRequestOutput {
            message_type: 0x40,
            op_id: 1,
            publish_key: Uuid128 { bytes: vec![1; 16] },
            envelope_id: Digest256 {
                bytes: local_head.envelope.as_bytes().to_vec(),
            },
            expected_generation: server_gen,
            expected_envelope: server_env.clone(),
            force: false,
        }),
    )
    .await;

    let mut objects_published = 0;
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::ObjectsNeeded(on) => {
                assert_eq!(on.op_id, 1);
                let mut batch_objects = Vec::new();
                for digest in &on.ids {
                    let oid = ObjectId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                    let obj = local_objects.get(oid).await.unwrap();
                    batch_objects.push(ObjectEntry {
                        id: digest.clone(),
                        data: obj.canonical_bytes.to_vec(),
                    });
                    objects_published += 1;
                }
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                        message_type: 0x21,
                        op_id: 1,
                        sequence: 0,
                        objects: batch_objects,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ObjectsComplete(oc) if oc.op_id == 1 => break,
            other => panic!("unexpected during publish objects: {other:?}"),
        }
    }

    // Content phase.
    let mut content_published = 0;
    let msg = recv_msg(&mut ws).await;
    match msg {
        WheatProtocolMessage::ContentNeeded(cn) => {
            assert_eq!(cn.op_id, 1);
            for digest in &cn.content_ids {
                let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                let data = local_content.get(id).unwrap();
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                        message_type: 0x44,
                        op_id: 1,
                        content_id: digest.clone(),
                        data,
                    }),
                )
                .await;
                content_published += 1;
            }
            send_msg(
                &mut ws,
                &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                    message_type: 0x46,
                    op_id: 1,
                }),
            )
            .await;
        }
        other => panic!("expected ContentNeeded, got {other:?}"),
    }

    // Commit.
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id: 1,
        }),
    )
    .await;

    let published_gen;
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(pr) => {
                assert_eq!(pr.op_id, 1);
                assert!(
                    matches!(pr.status, PublishResultStatus::PublishStatusOk(_)),
                    "publish should succeed: {:?} — {}",
                    pr.status,
                    pr.message,
                );
                published_gen = pr.new_generation;
                break;
            }
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }

    ws.close(None).await.ok();
    let publish_elapsed = publish_start.elapsed();

    eprintln!(
        "Publish: {} objects, {} content in {:?}",
        objects_published, content_published, publish_elapsed
    );

    // --- Clone from S3 ---
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();
    let (clone_objects, clone_content, clone_heads) = local_stores(&clone_path);

    let clone_start = std::time::Instant::now();

    let mut ws = connect_and_handshake(addr).await;
    let (clone_gen, clone_env) = open_repo(&mut ws, "bench/repo").await;
    assert_eq!(clone_gen, published_gen);

    let env_id = wheat_core::RepositoryEnvelopeId::from_digest(
        clone_env.bytes.as_slice().try_into().unwrap(),
    );

    let obj_count = client_fetch_objects(&mut ws, 2, &clone_objects, &clone_content, env_id).await;
    let content_count =
        client_fetch_content(&mut ws, 3, &clone_objects, &clone_content, &clone_env).await;

    let clone_elapsed = clone_start.elapsed();

    let head = RepositoryHead {
        generation: clone_gen,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    // Validate completeness.
    let reader = RepositoryReader::new(&clone_objects, &clone_content);
    let env = reader.envelope(env_id).await.unwrap();
    reader
        .validate_complete(env.state)
        .await
        .expect("cloned state from S3 should be complete");

    ws.close(None).await.ok();
    let _ = shutdown.send(());

    eprintln!();
    eprintln!("=== S3 Benchmark Results ===");
    eprintln!("Changes:         {num_changes}");
    eprintln!("Objects:         {obj_count_local}");
    eprintln!(
        "Publish (S3):    {:?} ({objects_published} objects, {content_published} content)",
        publish_elapsed
    );
    eprintln!(
        "Clone (S3):      {:?} ({obj_count} objects, {content_count} content)",
        clone_elapsed
    );
}
