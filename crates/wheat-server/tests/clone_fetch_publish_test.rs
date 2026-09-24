//! End-to-end tests for clone, fetch, and publish over the wheat native protocol.
//!
//! Each test starts an in-process wheat-server, then uses the client-side
//! protocol engine (reimplemented here as test helpers — the real client engine
//! lives in wheat-cli, which is a binary crate and can't be depended upon).

use std::collections::HashSet;
use std::net::SocketAddr;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use wheat_core::{
    ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind, FileValue,
    ObjectId, Repository, RevisionEdits, Tree,
};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BatchObjectStore, ContentStore, ImmutableObjectStore, LocalPublicationStore, PackedObjectStore,
    PersistedRepositoryMutator, PublicationStore, RepositoryHead, RepositoryPublisher,
    RepositoryReader, finalize_mutation_publication, prepare_mutation_publication,
};

// ---------------------------------------------------------------------------
// Reusable helpers (same as protocol_test.rs)
// ---------------------------------------------------------------------------

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

fn stores(root: &std::path::Path) -> (PackedObjectStore, ContentStore, LocalPublicationStore) {
    let objects = PackedObjectStore::open(root.join("storage")).unwrap();
    let content = objects.content_store();
    (
        objects,
        content,
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

async fn start_server(
    data_dir: &std::path::Path,
    repo_name: &str,
) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
    serve(Registry::single_named(repo_name.to_string(), data_dir).unwrap()).await
}

/// Serves the repository through handles the caller keeps, so the test can write to the
/// server's store while it runs; the packed store admits one opener per process.
async fn start_server_sharing(
    data_dir: &std::path::Path,
    repo_name: &str,
) -> (
    SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    (PackedObjectStore, ContentStore, LocalPublicationStore),
) {
    let (objects, content, heads) = stores(data_dir);
    let registry = Registry::single_with_stores(
        repo_name.to_string(),
        Box::new(objects.clone()),
        content.clone(),
        heads.clone(),
    );
    let (addr, shutdown) = serve(registry).await;
    (addr, shutdown, (objects, content, heads))
}

async fn serve(registry: Registry) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
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

async fn setup_repo(dir: &std::path::Path) -> Repository {
    let (objects, content, heads) = stores(dir);
    content.put(b"hello wheat").unwrap();

    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;

    repo.create_change(
        basis,
        basis,
        [],
        edit("greeting.txt", b"hello wheat"),
        ApplicationContext::new(&content),
    )
    .unwrap();

    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    publisher.initialize(&repo).await.unwrap();

    repo
}

// ---------------------------------------------------------------------------
// Client-side protocol helpers (thin wrappers for test use)
// ---------------------------------------------------------------------------

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

/// Client-side iterative root-first fetch. Returns total objects received.
async fn client_fetch_objects(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    op_id: u32,
    objects: &PackedObjectStore,
    content: &ContentStore,
    envelope_id: wheat_core::RepositoryEnvelopeId,
) -> usize {
    // Send FetchRequest.
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

                // Walk retained sets until convergence. Each round may
                // fetch objects whose children include new trie nodes;
                // those nodes may encode additional trie-key ObjectIds.
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

/// Client-side content fetch for all content referenced by the envelope.
async fn client_fetch_content(
    ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    op_id: u32,
    objects: &PackedObjectStore,
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
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn clone_round_trip() {
    // Set up a server with a populated repository.
    let server_dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(server_dir.path()).await;

    let (addr, shutdown) = start_server(server_dir.path(), "test/repo").await;

    // Clone into a fresh local store.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();

    let (clone_objects, clone_content, clone_heads) = stores(&clone_path);

    let mut ws = connect_and_handshake(addr).await;
    let (generation, envelope) = open_repo(&mut ws, "test/repo").await;
    let env_id = wheat_core::RepositoryEnvelopeId::from_digest(
        envelope.bytes.as_slice().try_into().unwrap(),
    );

    // Fetch all structural objects.
    let obj_count = client_fetch_objects(&mut ws, 1, &clone_objects, &clone_content, env_id).await;
    assert!(obj_count > 0, "should have fetched some objects");

    // Fetch content.
    let content_count =
        client_fetch_content(&mut ws, 2, &clone_objects, &clone_content, &envelope).await;
    assert_eq!(content_count, 1, "should have fetched greeting.txt content");
    let head = RepositoryHead {
        generation,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    // Verify the clone: read the envelope, FULLY validate the closure.
    let reader = RepositoryReader::new(&clone_objects, &clone_content);
    let env = reader.envelope(env_id).await.unwrap();
    reader
        .validate_complete(env.state)
        .await
        .expect("cloned state should be complete");

    // Content should be fetchable locally.
    let greeting = clone_content.get(ContentId::of(b"hello wheat")).unwrap();
    assert_eq!(greeting, b"hello wheat");

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn publish_round_trip() {
    // Set up server with an initial repo.
    let server_dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(server_dir.path()).await;

    let (addr, shutdown) = start_server(server_dir.path(), "test/repo").await;

    // Clone first.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();
    let (clone_objects, clone_content, clone_heads) = stores(&clone_path);

    let mut ws = connect_and_handshake(addr).await;
    let (generation, envelope) = open_repo(&mut ws, "test/repo").await;
    let env_id = wheat_core::RepositoryEnvelopeId::from_digest(
        envelope.bytes.as_slice().try_into().unwrap(),
    );

    client_fetch_objects(&mut ws, 1, &clone_objects, &clone_content, env_id).await;
    client_fetch_content(&mut ws, 2, &clone_objects, &clone_content, &envelope).await;

    let head = RepositoryHead {
        generation,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    // Create a change locally using the mutator (same as wheat-cli does).
    clone_content.put(b"goodbye wheat").unwrap();

    let reader = RepositoryReader::new(&clone_objects, &clone_content);
    let envelope_rec = reader.envelope(env_id).await.unwrap();
    let state = reader.state(envelope_rec.state).await.unwrap();

    let basis_view = state.base_view;
    let effective_view = basis_view; // For a fresh clone, effective == basis.

    let proposal = wheat_core::ChangeProposal {
        id: wheat_core::ChangeId::new(),
        metadata: wheat_core::ChangeMetadata {
            title: "farewell".to_string(),
            description: String::new(),
            created_at: wheat_core::Timestamp {
                seconds: 1000000,
                offset_minutes: 0,
            },
            creator: wheat_core::Actor {
                name: "test".into(),
                email: "test@test".into(),
            },
            provenance: None,
        },
        dependencies: std::collections::BTreeSet::new(),
        revision_metadata: wheat_core::RevisionMetadata {
            message: "add farewell".to_string(),
            authored_at: wheat_core::Timestamp {
                seconds: 1000000,
                offset_minutes: 0,
            },
            committed_at: wheat_core::Timestamp {
                seconds: 1000000,
                offset_minutes: 0,
            },
            author: wheat_core::Actor {
                name: "test".into(),
                email: "test@test".into(),
            },
            committer: wheat_core::Actor {
                name: "test".into(),
                email: "test@test".into(),
            },
            provenance: None,
        },
        edits: edit("farewell.txt", b"goodbye wheat"),
    };

    let objects_batch = BatchObjectStore::new(&clone_objects, 1024, 8 * 1024 * 1024);
    let mutator = PersistedRepositoryMutator::new(&objects_batch, &clone_content, &clone_heads);
    let (prepared, _outcome) = mutator
        .create_on_frontier(head, basis_view, effective_view, proposal)
        .await
        .unwrap();
    let _resulting_view = prepared.resulting_view();
    let publication =
        prepare_mutation_publication(&objects_batch, &clone_content, &clone_heads, head, prepared)
            .await
            .unwrap();
    objects_batch.flush().await.unwrap();
    finalize_mutation_publication(&clone_heads, publication).unwrap();

    let new_head = clone_heads.read().unwrap();

    // Verify the cloned state is complete BEFORE creating a local change.
    {
        let verify_reader = RepositoryReader::new(&clone_objects, &clone_content);
        let verify_env = verify_reader.envelope(env_id).await.unwrap();
        verify_reader
            .validate_complete(verify_env.state)
            .await
            .expect("cloned state should be complete");
    }

    // Now publish to the server.
    ws.close(None).await.ok();
    let mut ws = connect_and_handshake(addr).await;
    let (server_gen, server_env) = open_repo(&mut ws, "test/repo").await;
    assert_eq!(
        server_gen, generation,
        "server should be at original generation"
    );

    // Send PublishRequest.
    let local_envelope = new_head.envelope;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::PublishRequest(PublishRequestOutput {
            message_type: 0x40,
            op_id: 3,
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
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::ObjectsNeeded(on) => {
                assert_eq!(on.op_id, 3);
                let mut batch_objects = Vec::new();
                for digest in &on.ids {
                    let oid = ObjectId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                    let obj = clone_objects.get(oid).await.unwrap();
                    batch_objects.push(ObjectEntry {
                        id: digest.clone(),
                        data: obj.canonical_bytes.to_vec(),
                    });
                }
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ObjectBatch(ObjectBatchOutput {
                        message_type: 0x21,
                        op_id: 3,
                        sequence: 0,
                        objects: batch_objects,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ObjectsComplete(oc) => {
                assert_eq!(oc.op_id, 3);
                break;
            }
            other => panic!("unexpected during publish objects: {other:?}"),
        }
    }

    // Content phase.
    let msg = recv_msg(&mut ws).await;
    match msg {
        WheatProtocolMessage::ContentNeeded(cn) => {
            assert_eq!(cn.op_id, 3);
            for digest in &cn.content_ids {
                let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                let data = clone_content.get(id).unwrap();
                send_msg(
                    &mut ws,
                    &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
                        message_type: 0x44,
                        op_id: 3,
                        content_id: digest.clone(),
                        data,
                    }),
                )
                .await;
            }
            send_msg(
                &mut ws,
                &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                    message_type: 0x46,
                    op_id: 3,
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
            op_id: 3,
        }),
    )
    .await;

    // Wait for result.
    loop {
        let msg = recv_msg(&mut ws).await;
        match msg {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(pr) => {
                assert_eq!(pr.op_id, 3);
                assert!(
                    matches!(pr.status, PublishResultStatus::PublishStatusOk(_)),
                    "publish should succeed, got: {:?}, message: {}",
                    pr.status,
                    pr.message,
                );
                assert!(pr.new_generation > generation, "generation should advance");
                break;
            }
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }

    // Verify: reconnect and check the server's head advanced.
    ws.close(None).await.ok();
    let mut ws = connect_and_handshake(addr).await;
    let (new_server_gen, _new_server_env) = open_repo(&mut ws, "test/repo").await;
    assert!(
        new_server_gen > generation,
        "server generation should have advanced from {generation} to {new_server_gen}"
    );

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}

#[tokio::test]
async fn fetch_after_server_advance() {
    // Set up server.
    let server_dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(server_dir.path()).await;

    let (addr, shutdown, server_stores) =
        start_server_sharing(server_dir.path(), "test/repo").await;

    // Clone.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();
    let (clone_objects, clone_content, clone_heads) = stores(&clone_path);

    let mut ws = connect_and_handshake(addr).await;
    let (gen1, env1) = open_repo(&mut ws, "test/repo").await;
    let env1_id =
        wheat_core::RepositoryEnvelopeId::from_digest(env1.bytes.as_slice().try_into().unwrap());
    client_fetch_objects(&mut ws, 1, &clone_objects, &clone_content, env1_id).await;
    client_fetch_content(&mut ws, 2, &clone_objects, &clone_content, &env1).await;
    let head1 = RepositoryHead {
        generation: gen1,
        envelope: env1_id,
    };
    clone_heads.initialize(head1).unwrap();
    ws.close(None).await.ok();

    // Advance the server by publishing a second change directly to the server's store.
    {
        let (srv_objects, srv_content, srv_heads) = server_stores;
        srv_content.put(b"server change").unwrap();

        let srv_head = srv_heads.read().unwrap();
        let reader = RepositoryReader::new(&srv_objects, &srv_content);
        let env = reader.envelope(srv_head.envelope).await.unwrap();
        let state = reader.state(env.state).await.unwrap();

        let proposal = wheat_core::ChangeProposal {
            id: wheat_core::ChangeId::new(),
            metadata: wheat_core::ChangeMetadata {
                title: "server update".to_string(),
                description: String::new(),
                created_at: wheat_core::Timestamp {
                    seconds: 1000000,
                    offset_minutes: 0,
                },
                creator: wheat_core::Actor {
                    name: "server".into(),
                    email: "server@test".into(),
                },
                provenance: None,
            },
            dependencies: std::collections::BTreeSet::new(),
            revision_metadata: wheat_core::RevisionMetadata {
                message: "add server.txt".to_string(),
                authored_at: wheat_core::Timestamp {
                    seconds: 1000000,
                    offset_minutes: 0,
                },
                committed_at: wheat_core::Timestamp {
                    seconds: 1000000,
                    offset_minutes: 0,
                },
                author: wheat_core::Actor {
                    name: "server".into(),
                    email: "server@test".into(),
                },
                committer: wheat_core::Actor {
                    name: "server".into(),
                    email: "server@test".into(),
                },
                provenance: None,
            },
            edits: edit("server.txt", b"server change"),
        };

        let srv_batch = BatchObjectStore::new(&srv_objects, 1024, 8 * 1024 * 1024);
        let mutator = PersistedRepositoryMutator::new(&srv_batch, &srv_content, &srv_heads);
        let (prepared, _outcome) = mutator
            .create_on_frontier(srv_head, state.base_view, state.base_view, proposal)
            .await
            .unwrap();
        let publication =
            prepare_mutation_publication(&srv_batch, &srv_content, &srv_heads, srv_head, prepared)
                .await
                .unwrap();
        srv_batch.flush().await.unwrap();
        finalize_mutation_publication(&srv_heads, publication).unwrap();
    }

    // Now fetch the update.
    let mut ws = connect_and_handshake(addr).await;
    let (gen2, env2) = open_repo(&mut ws, "test/repo").await;
    let env2_id =
        wheat_core::RepositoryEnvelopeId::from_digest(env2.bytes.as_slice().try_into().unwrap());
    assert!(gen2 > gen1, "server should have advanced");

    let obj_count = client_fetch_objects(&mut ws, 3, &clone_objects, &clone_content, env2_id).await;
    assert!(obj_count > 0, "should fetch new objects");

    let content_count =
        client_fetch_content(&mut ws, 4, &clone_objects, &clone_content, &env2).await;
    assert!(content_count > 0, "should fetch new content");

    // Update clone head.
    let env2_id =
        wheat_core::RepositoryEnvelopeId::from_digest(env2.bytes.as_slice().try_into().unwrap());
    let head2 = RepositoryHead {
        generation: gen2,
        envelope: env2_id,
    };
    clone_heads.compare_and_advance(head1, head2).unwrap();

    // Verify the new content is available locally.
    let data = clone_content.get(ContentId::of(b"server change")).unwrap();
    assert_eq!(data, b"server change");

    ws.close(None).await.ok();
    let _ = shutdown.send(());
}
