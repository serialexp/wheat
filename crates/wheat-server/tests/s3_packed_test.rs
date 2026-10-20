//! End-to-end test: publish and clone through an S3-backed wheat-server.
//!
//! Requires WHEAT_TEST_S3_* env vars (e.g. via scripts/wheat-packed-segment-bench).
//! Run with:
//!   cargo test -p wheat-server --test s3_packed_test -- --ignored --nocapture

mod support;

use std::net::SocketAddr;

use wheat_core::{ChangeProposal, ContentId, Repository, RevisionEdits, Tree};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BatchObjectStore, ContentStore, ImmutableObjectStore, LocalPublicationStore, LooseObjectStore,
    PersistedRepositoryMutator, PublicationStore, RemotePackedStore, RemotePackedStoreConfig,
    RepositoryHead, RepositoryPublisher, RepositoryReader, S3ObjectStoreConfig,
    finalize_mutation_publication, prepare_mutation_publication,
};

use support::{
    Upload, Ws, connect_and_handshake, content_batch, edit, fetch_content, fetch_objects,
    open_repo, recv_msg, request_publish, send_msg, upload_objects,
};

// ---------------------------------------------------------------------------
// Helpers
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
    let heads = LocalPublicationStore::new(head_dir.join("repository.head"));
    let registry = Registry::single_with_stores(repo_name.to_string(), Box::new(store), heads);

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

/// Answer the server's `ContentNeeded` rounds for publish op 1 until it sends
/// `ContentComplete`. Returns how many blobs went up.
async fn upload_needed_content(ws: &mut Ws, content: &ContentStore) -> usize {
    let mut sent = 0;
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ContentNeeded(cn) => {
                assert_eq!(cn.op_id, 1);
                let mut blobs = Vec::new();
                for digest in &cn.content_ids {
                    let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                    blobs.push((id, content.get(id).unwrap()));
                }
                sent += blobs.len();
                send_msg(ws, &content_batch(1, blobs)).await;
                send_msg(
                    ws,
                    &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                        message_type: 0x46,
                        op_id: 1,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ContentComplete(cc) => {
                assert_eq!(cc.op_id, 1);
                return sent;
            }
            other => panic!("expected ContentNeeded or ContentComplete, got {other:?}"),
        }
    }
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
    request_publish(&mut ws, 1, local_envelope, (server_gen, server_env), false).await;
    let rounds = upload_objects(&mut ws, 1, &local_objects, Upload::Whole).await;
    let objects_sent: usize = rounds.iter().sum();
    eprintln!("published {objects_sent} objects to S3 server");

    // Content phase.
    let content_sent = upload_needed_content(&mut ws, &local_content).await;
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
    let (clone_gen, env_id) = open_repo(&mut ws, "bench/repo").await;
    assert_eq!(
        clone_gen, published_generation,
        "server should be at published generation"
    );

    let clone_start = std::time::Instant::now();

    let fetched = fetch_objects(&mut ws, 2, &clone_objects, env_id, None).await;
    assert_eq!(fetched.wants, 0, "a clone needs no wants: {fetched:?}");
    let obj_count = fetched.pushed;
    let content_count = fetch_content(&mut ws, 3, &clone_objects, &clone_content, env_id).await;

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

    request_publish(
        &mut ws,
        1,
        local_head.envelope,
        (server_gen, server_env),
        false,
    )
    .await;
    let rounds = upload_objects(&mut ws, 1, &local_objects, Upload::Whole).await;
    let objects_published: usize = rounds.iter().sum();

    // Content phase.
    let content_published = upload_needed_content(&mut ws, &local_content).await;

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
    let (clone_gen, env_id) = open_repo(&mut ws, "bench/repo").await;
    assert_eq!(clone_gen, published_gen);

    let fetched = fetch_objects(&mut ws, 2, &clone_objects, env_id, None).await;
    assert_eq!(fetched.wants, 0, "a clone needs no wants: {fetched:?}");
    let obj_count = fetched.pushed;
    let content_count = fetch_content(&mut ws, 3, &clone_objects, &clone_content, env_id).await;

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
