//! End-to-end tests for clone, fetch, and publish over the wheat native protocol.
//!
//! Each test starts an in-process wheat-server and talks to it with the small
//! client in `support`.

mod support;

use std::net::SocketAddr;

use wheat_core::{ApplicationContext, ContentId, Repository, Tree};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BatchObjectStore, ContentStore, LocalPublicationStore, PackedObjectStore,
    PersistedRepositoryMutator, PublicationStore, RepositoryHead, RepositoryPublisher,
    RepositoryReader, finalize_mutation_publication, prepare_mutation_publication,
};

use support::{
    Upload, Ws, connect_and_handshake, edit, fetch_content, fetch_objects, open_repo, recv_msg,
    request_publish, send_msg, upload_objects,
};

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
    let (generation, env_id) = open_repo(&mut ws, "test/repo").await;

    // A clone gets everything in the push and has nothing left to ask for.
    let fetched = fetch_objects(&mut ws, 1, &clone_objects, env_id, None).await;
    assert!(fetched.pushed > 0, "should have fetched some objects");
    assert_eq!((fetched.pulled, fetched.wants), (0, 0), "{fetched:?}");
    assert!(!fetched.incremental);

    // Fetch content.
    let content_count = fetch_content(&mut ws, 2, &clone_objects, &clone_content, env_id).await;
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

/// The op id of every publish in these tests.
const PUBLISH_OP: u32 = 3;

/// A publish driven up to its content phase: the server's `ContentNeeded`
/// has arrived and nothing has been uploaded yet.
struct ContentPhase {
    ws: Ws,
    /// The content the server asked for.
    needed: Vec<Digest256>,
    /// The publishing clone's content store.
    content: ContentStore,
    /// The server's generation before the publish.
    generation: u64,
    addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    _server_dir: tempfile::TempDir,
    _clone_dir: tempfile::TempDir,
}

/// Start a server, clone it, add `farewell.txt` holding `farewell` as a new
/// change, and publish it until the server asks for content.
async fn publish_to_content_phase(farewell: &[u8]) -> ContentPhase {
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
    let (generation, env_id) = open_repo(&mut ws, "test/repo").await;

    fetch_objects(&mut ws, 1, &clone_objects, env_id, None).await;
    fetch_content(&mut ws, 2, &clone_objects, &clone_content, env_id).await;

    let head = RepositoryHead {
        generation,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    // Create a change locally using the mutator (same as wheat-cli does).
    clone_content.put(farewell).unwrap();

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
        edits: edit("farewell.txt", farewell),
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

    request_publish(
        &mut ws,
        PUBLISH_OP,
        new_head.envelope,
        (server_gen, server_env),
        false,
    )
    .await;
    upload_objects(&mut ws, PUBLISH_OP, &clone_objects, Upload::Whole).await;

    // Content phase.
    let needed = match recv_msg(&mut ws).await {
        WheatProtocolMessage::ContentNeeded(cn) => {
            assert_eq!(cn.op_id, PUBLISH_OP);
            cn.content_ids
        }
        other => panic!("expected ContentNeeded, got {other:?}"),
    };
    ContentPhase {
        ws,
        needed,
        content: clone_content,
        generation,
        addr,
        shutdown,
        _server_dir: server_dir,
        _clone_dir: clone_dir,
    }
}

async fn upload_whole(ws: &mut Ws, digest: &Digest256, data: Vec<u8>) {
    send_msg(
        ws,
        &WheatProtocolMessage::ContentUpload(ContentUploadOutput {
            message_type: 0x44,
            op_id: PUBLISH_OP,
            content_id: digest.clone(),
            data,
        }),
    )
    .await;
}

async fn upload_chunk(ws: &mut Ws, digest: &Digest256, offset: u64, data: &[u8], is_final: bool) {
    send_msg(
        ws,
        &WheatProtocolMessage::ContentUploadChunk(ContentUploadChunkOutput {
            message_type: 0x45,
            op_id: PUBLISH_OP,
            content_id: digest.clone(),
            offset,
            data: data.to_vec(),
            is_final,
        }),
    )
    .await;
}

async fn upload_complete(ws: &mut Ws) {
    send_msg(
        ws,
        &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
            message_type: 0x46,
            op_id: PUBLISH_OP,
        }),
    )
    .await;
}

/// Wait for the server to say it has all the content, then ask it to commit and
/// return its answer.
async fn commit_publish(ws: &mut Ws) -> PublishResultOutput {
    match recv_msg(ws).await {
        WheatProtocolMessage::ContentComplete(cc) => assert_eq!(cc.op_id, PUBLISH_OP),
        other => panic!("expected ContentComplete, got {other:?}"),
    }
    send_msg(
        ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id: PUBLISH_OP,
        }),
    )
    .await;
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(pr) => {
                assert_eq!(pr.op_id, PUBLISH_OP);
                return pr;
            }
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }
}

fn assert_published(pr: &PublishResultOutput, generation: u64) {
    assert!(
        matches!(pr.status, PublishResultStatus::PublishStatusOk(_)),
        "publish should succeed, got: {:?}, message: {}",
        pr.status,
        pr.message,
    );
    assert!(pr.new_generation > generation, "generation should advance");
}

/// The server refuses the upload as soon as it breaks a rule, without waiting
/// for a commit, and its head stays where it was.
async fn assert_upload_refused(phase: &mut ContentPhase, reason: &str) {
    let pr = match recv_msg(&mut phase.ws).await {
        WheatProtocolMessage::PublishResult(pr) => pr,
        other => panic!("expected PublishResult, got {other:?}"),
    };
    assert_eq!(pr.op_id, PUBLISH_OP);
    assert!(
        matches!(pr.status, PublishResultStatus::PublishStatusRejected(_)),
        "upload should be refused, got: {:?}, message: {}",
        pr.status,
        pr.message,
    );
    assert!(
        pr.message.contains(reason),
        "refusal {:?} should mention {reason:?}",
        pr.message
    );
    assert_eq!(pr.new_generation, phase.generation);

    let mut ws = connect_and_handshake(phase.addr).await;
    let (server_gen, _) = open_repo(&mut ws, "test/repo").await;
    assert_eq!(server_gen, phase.generation, "head must not move");
    ws.close(None).await.ok();
}

#[tokio::test]
async fn publish_round_trip() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    let ws = &mut phase.ws;
    for digest in &phase.needed {
        let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
        let data = phase.content.get(id).unwrap();
        upload_whole(ws, digest, data).await;
    }
    upload_complete(ws).await;
    let pr = commit_publish(ws).await;
    let generation = phase.generation;
    assert_published(&pr, generation);

    // Verify: reconnect and check the server's head advanced.
    phase.ws.close(None).await.ok();
    let mut ws = connect_and_handshake(phase.addr).await;
    let (new_server_gen, _new_server_env) = open_repo(&mut ws, "test/repo").await;
    assert!(
        new_server_gen > generation,
        "server generation should have advanced from {generation} to {new_server_gen}"
    );

    ws.close(None).await.ok();
    let _ = phase.shutdown.send(());
}

#[tokio::test]
async fn publish_reassembles_chunked_content() {
    let farewell = b"goodbye wheat, in three pieces";
    let mut phase = publish_to_content_phase(farewell).await;
    let digest = Digest256 {
        bytes: ContentId::of(farewell).as_bytes().to_vec(),
    };
    assert_eq!(phase.needed, vec![digest.clone()]);

    let ws = &mut phase.ws;
    upload_chunk(ws, &digest, 0, &farewell[..7], false).await;
    upload_chunk(ws, &digest, 7, &farewell[7..20], false).await;
    upload_chunk(ws, &digest, 20, &farewell[20..], true).await;
    upload_complete(ws).await;
    let pr = commit_publish(ws).await;
    assert_published(&pr, phase.generation);

    // A fresh clone gets the reassembled bytes back.
    let clone_dir = tempfile::tempdir().unwrap();
    let (clone_objects, clone_content, _) = stores(clone_dir.path());
    let mut ws = connect_and_handshake(phase.addr).await;
    let (_, env_id) = open_repo(&mut ws, "test/repo").await;
    fetch_objects(&mut ws, 1, &clone_objects, env_id, None).await;
    fetch_content(&mut ws, 2, &clone_objects, &clone_content, env_id).await;
    assert_eq!(
        clone_content.get(ContentId::of(farewell)).unwrap(),
        farewell
    );
    ws.close(None).await.ok();
    let _ = phase.shutdown.send(());
}

#[tokio::test]
async fn publish_refuses_content_it_did_not_ask_for() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    let stray = b"not requested".to_vec();
    let digest = Digest256 {
        bytes: ContentId::of(&stray).as_bytes().to_vec(),
    };
    upload_whole(&mut phase.ws, &digest, stray).await;
    assert_upload_refused(&mut phase, "was not requested").await;
}

#[tokio::test]
async fn publish_refuses_content_sent_twice() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    let digest = phase.needed[0].clone();
    upload_whole(&mut phase.ws, &digest, b"goodbye wheat".to_vec()).await;
    upload_chunk(&mut phase.ws, &digest, 0, b"goodbye wheat", true).await;
    assert_upload_refused(&mut phase, "was not requested").await;
}

#[tokio::test]
async fn publish_refuses_a_chunk_that_skips_ahead() {
    let farewell = b"goodbye wheat";
    let mut phase = publish_to_content_phase(farewell).await;
    let digest = phase.needed[0].clone();
    upload_chunk(&mut phase.ws, &digest, 0, &farewell[..4], false).await;
    upload_chunk(&mut phase.ws, &digest, 5, &farewell[5..], true).await;
    assert_upload_refused(&mut phase, "offset 5, expected 4").await;
}

#[tokio::test]
async fn publish_refuses_an_upload_missing_content() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    upload_complete(&mut phase.ws).await;
    assert_upload_refused(&mut phase, "were not sent").await;
}

#[tokio::test]
async fn publish_refuses_an_unfinished_chunked_upload() {
    let farewell = b"goodbye wheat";
    let mut phase = publish_to_content_phase(farewell).await;
    let digest = phase.needed[0].clone();
    upload_chunk(&mut phase.ws, &digest, 0, &farewell[..4], false).await;
    upload_complete(&mut phase.ws).await;
    assert_upload_refused(&mut phase, "ended before its final chunk").await;
}

#[tokio::test]
async fn publish_refuses_content_that_does_not_match_its_id() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    let digest = phase.needed[0].clone();
    upload_whole(&mut phase.ws, &digest, b"something else".to_vec()).await;
    assert_upload_refused(&mut phase, "identity mismatch").await;
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
    let (gen1, env1_id) = open_repo(&mut ws, "test/repo").await;
    fetch_objects(&mut ws, 1, &clone_objects, env1_id, None).await;
    fetch_content(&mut ws, 2, &clone_objects, &clone_content, env1_id).await;
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
    let (gen2, env2_id) = open_repo(&mut ws, "test/repo").await;
    assert!(gen2 > gen1, "server should have advanced");

    // Only what the first fetch lacks comes over, and nothing is left to ask for.
    let fetched = fetch_objects(&mut ws, 3, &clone_objects, env2_id, Some(env1_id)).await;
    assert!(fetched.incremental, "{fetched:?}");
    assert!(fetched.pushed > 0, "should fetch new objects");
    assert_eq!((fetched.pulled, fetched.wants), (0, 0), "{fetched:?}");
    let reader = RepositoryReader::new(&clone_objects, &clone_content);
    let state = reader.envelope(env2_id).await.unwrap().state;

    let content_count = fetch_content(&mut ws, 4, &clone_objects, &clone_content, env2_id).await;
    assert!(content_count > 0, "should fetch new content");
    reader
        .validate_complete(state)
        .await
        .expect("fetched state should be complete");

    // A full fetch of the same envelope sends more.
    let fresh_dir = tempfile::tempdir().unwrap();
    let (fresh_objects, _, _) = stores(fresh_dir.path());
    let full = fetch_objects(&mut ws, 5, &fresh_objects, env2_id, None).await;
    assert!(
        fetched.pushed < full.pushed,
        "incremental {fetched:?} vs full {full:?}"
    );

    // Update clone head.
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
