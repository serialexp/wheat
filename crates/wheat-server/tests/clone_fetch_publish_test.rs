//! End-to-end tests for clone, fetch, and publish over the wheat native protocol.
//!
//! Each test starts an in-process wheat-server and talks to it with the small
//! client in `support`.

use crate::support;

use std::net::SocketAddr;

use wheat_core::{ApplicationContext, ContentId, Repository, Tree};
use wheat_protocol::*;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BatchObjectStore, ContentStore, LocalPublicationStore, PackedObjectStore, PackedStoreTrace,
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

    let mut ws = connect_and_handshake(addr, "test/repo").await;
    let (generation, env_id) = open_repo(&mut ws).await;

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
const PUBLISH_OP: u64 = 3;

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
    /// The server store's counters.
    trace: PackedStoreTrace,
    addr: SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    _server_dir: tempfile::TempDir,
    _clone_dir: tempfile::TempDir,
}

/// Start a server, clone it, add `farewell.txt` holding `farewell` as a new
/// change, and publish it until the server asks for content.
async fn publish_to_content_phase(farewell: &[u8]) -> ContentPhase {
    publish_files_to_content_phase(&[("farewell.txt", farewell)], &[]).await
}

/// [`publish_to_content_phase`] for a change adding `files`, with `seeded` stored on
/// the server before it starts.
async fn publish_files_to_content_phase(files: &[(&str, &[u8])], seeded: &[&[u8]]) -> ContentPhase {
    // Set up server with an initial repo.
    let server_dir = tempfile::tempdir().unwrap();
    let _repo = setup_repo(server_dir.path()).await;

    let (server_store, trace) =
        PackedObjectStore::open_traced(server_dir.path().join("storage")).unwrap();
    for blob in seeded {
        server_store.content_store().put(blob).unwrap();
    }
    let (addr, shutdown) = serve(Registry::single_with_stores(
        "test/repo".to_string(),
        Box::new(server_store),
        LocalPublicationStore::new(server_dir.path().join("repository.head")),
    ))
    .await;

    // Clone first.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    std::fs::create_dir(&clone_path).unwrap();
    let (clone_objects, clone_content, clone_heads) = stores(&clone_path);

    let mut ws = connect_and_handshake(addr, "test/repo").await;
    let (generation, env_id) = open_repo(&mut ws).await;

    fetch_objects(&mut ws, 1, &clone_objects, env_id, None).await;
    fetch_content(&mut ws, 2, &clone_objects, &clone_content, env_id).await;

    let head = RepositoryHead {
        generation,
        envelope: env_id,
    };
    clone_heads.initialize(head).unwrap();

    // Create a change locally using the mutator (same as wheat-cli does).
    for (_, data) in files {
        clone_content.put(data).unwrap();
    }

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
        edits: support::edits(files.iter().copied()),
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
    let mut ws = connect_and_handshake(addr, "test/repo").await;
    let (server_gen, server_env) = open_repo(&mut ws).await;
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
        trace,
        addr,
        shutdown,
        _server_dir: server_dir,
        _clone_dir: clone_dir,
    }
}

async fn upload_whole(ws: &mut Ws, digest: &Digest256, data: Vec<u8>) {
    send_msg(
        ws,
        &WheatProtocolMessage::ContentBatch(ContentBatchOutput {
            message_type: 0x31,
            op_id: PUBLISH_OP,
            sequence: 0,
            contents: vec![ContentEntry {
                id: digest.clone(),
                data,
            }],
        }),
    )
    .await;
}

async fn upload_chunk(ws: &mut Ws, digest: &Digest256, offset: u64, data: &[u8], is_final: bool) {
    send_msg(
        ws,
        &WheatProtocolMessage::ContentChunk(ContentChunkOutput {
            message_type: 0x32,
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

    let mut ws = connect_and_handshake(phase.addr, "test/repo").await;
    let (server_gen, _) = open_repo(&mut ws).await;
    assert_eq!(server_gen, phase.generation, "head must not move");
    ws.close(None).await.ok();
}

#[tokio::test]
async fn publish_round_trip() {
    let mut phase = publish_to_content_phase(b"goodbye wheat").await;
    let appends = || phase.trace.snapshot().unwrap().durable_appends;
    let before = appends();
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
    // The objects and the content go into the store together.
    assert_eq!(appends() - before, 1);

    // Verify: reconnect and check the server's head advanced.
    phase.ws.close(None).await.ok();
    let mut ws = connect_and_handshake(phase.addr, "test/repo").await;
    let (new_server_gen, _new_server_env) = open_repo(&mut ws).await;
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
    let mut ws = connect_and_handshake(phase.addr, "test/repo").await;
    let (_, env_id) = open_repo(&mut ws).await;
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
    assert_upload_refused(&mut phase, "answered with nothing").await;
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

/// Bytes that do not repeat, from `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 32) as u8
        })
        .collect()
}

/// A blob over the chunking threshold, cut into several chunks.
const LONG: usize = wheat_core::CHUNKED_CONTENT_THRESHOLD as usize + 4 * 1024 * 1024 + 7;

fn digest_of(id: ContentId) -> Digest256 {
    Digest256 {
        bytes: id.as_bytes().to_vec(),
    }
}

/// The chunks `content` stores the long blob `id` as.
fn chunk_ids(content: &ContentStore, id: ContentId) -> Vec<ContentId> {
    match content.chunk_manifest(id).unwrap().unwrap().entries {
        wheat_core::ContentManifestEntries::Chunks(chunks) => {
            chunks.iter().map(|chunk| chunk.id).collect()
        }
        wheat_core::ContentManifestEntries::Parts(_) => panic!("expected chunks"),
    }
}

/// Sends the manifest `content` stores for `of` as the manifest of `id`.
async fn upload_manifest(ws: &mut Ws, content: &ContentStore, id: ContentId, of: ContentId) {
    let manifest = content.chunk_manifest(of).unwrap().unwrap();
    let encoded = wheat_core::encode_content_chunk_manifest(&manifest).unwrap();
    send_msg(ws, &content_manifest(PUBLISH_OP, digest_of(id), encoded)).await;
}

/// The next `ContentNeeded`'s IDs.
async fn next_needed(ws: &mut Ws) -> Vec<ContentId> {
    match recv_msg(ws).await {
        WheatProtocolMessage::ContentNeeded(cn) => {
            assert_eq!(cn.op_id, PUBLISH_OP);
            cn.content_ids
                .iter()
                .map(|d| ContentId::from_digest(d.bytes.as_slice().try_into().unwrap()))
                .collect()
        }
        other => panic!("expected ContentNeeded, got {other:?}"),
    }
}

/// Uploads each of `ids` from `content` whole, then ends the round.
async fn upload_round(ws: &mut Ws, content: &ContentStore, ids: &[ContentId]) {
    for &id in ids {
        upload_whole(ws, &digest_of(id), content.get(id).unwrap()).await;
    }
    upload_complete(ws).await;
}

fn sorted(mut ids: Vec<ContentId>) -> Vec<ContentId> {
    ids.sort();
    ids.dedup();
    ids
}

#[tokio::test]
async fn publish_takes_a_long_blob_as_its_manifest_then_its_chunks() {
    let long = noise(1, LONG);
    let id = ContentId::of(&long);
    let mut phase = publish_to_content_phase(&long).await;
    assert_eq!(phase.needed, vec![digest_of(id)]);
    let content = phase.content.clone();
    let chunks = chunk_ids(&content, id);
    assert!(chunks.len() > 4, "{} chunks", chunks.len());

    let ws = &mut phase.ws;
    upload_manifest(ws, &content, id, id).await;
    upload_complete(ws).await;
    // The server has none of the chunks, so it asks for every one, each once.
    let needed = next_needed(ws).await;
    assert_eq!(needed.len(), chunks.len());
    assert_eq!(sorted(needed.clone()), sorted(chunks));
    upload_round(ws, &content, &needed).await;
    let pr = commit_publish(ws).await;
    assert_published(&pr, phase.generation);

    // The server serves the blob as the manifest it was sent.
    let mut ws = connect_and_handshake(phase.addr, "test/repo").await;
    open_repo(&mut ws).await;
    send_msg(
        &mut ws,
        &WheatProtocolMessage::ContentFetchRequest(ContentFetchRequestOutput {
            message_type: 0x30,
            op_id: 9,
            content_ids: vec![digest_of(id)],
        }),
    )
    .await;
    let expected =
        wheat_core::encode_content_chunk_manifest(&content.chunk_manifest(id).unwrap().unwrap())
            .unwrap();
    match recv_msg(&mut ws).await {
        WheatProtocolMessage::ContentManifest(sent) => {
            assert_eq!(sent.content_id, digest_of(id));
            assert!(sent.manifest == expected, "the manifest as sent");
        }
        other => panic!("expected ContentManifest, got {other:?}"),
    }
    ws.close(None).await.ok();
    let _ = phase.shutdown.send(());
}

#[tokio::test]
async fn a_small_edit_to_a_long_blob_asks_only_for_the_chunks_it_changed() {
    let before = noise(1, LONG);
    let mut after = before.clone();
    after[LONG / 2] ^= 0xff;
    let id = ContentId::of(&after);
    let mut phase = publish_files_to_content_phase(&[("farewell.txt", &after)], &[&before]).await;
    assert_eq!(phase.needed, vec![digest_of(id)]);
    let content = phase.content.clone();
    // The chunks the server holds are those of `before`.
    let held = content.put(&before).unwrap();
    let held = chunk_ids(&content, held);
    let changed: Vec<ContentId> = chunk_ids(&content, id)
        .into_iter()
        .filter(|chunk| !held.contains(chunk))
        .collect();
    assert!(
        (1..=2).contains(&changed.len()),
        "{} changed chunks",
        changed.len()
    );

    let ws = &mut phase.ws;
    upload_manifest(ws, &content, id, id).await;
    upload_complete(ws).await;
    let needed = next_needed(ws).await;
    assert_eq!(sorted(needed.clone()), sorted(changed));
    upload_round(ws, &content, &needed).await;
    let pr = commit_publish(ws).await;
    assert_published(&pr, phase.generation);
    let _ = phase.shutdown.send(());
}

#[tokio::test]
async fn publish_asks_again_for_a_long_blob_left_out_of_an_answer() {
    let (first, second) = (noise(1, LONG), noise(2, LONG));
    let (first_id, second_id) = (ContentId::of(&first), ContentId::of(&second));
    let mut phase =
        publish_files_to_content_phase(&[("a.bin", &first), ("b.bin", &second)], &[]).await;
    assert_eq!(
        sorted(
            phase
                .needed
                .iter()
                .map(|d| ContentId::from_digest(d.bytes.as_slice().try_into().unwrap()))
                .collect()
        ),
        sorted(vec![first_id, second_id])
    );
    let content = phase.content.clone();
    let ws = &mut phase.ws;
    // As a sender past its manifest budget would: the first only.
    upload_manifest(ws, &content, first_id, first_id).await;
    upload_complete(ws).await;
    let needed = next_needed(ws).await;
    assert_eq!(
        sorted(needed.clone()),
        sorted(chunk_ids(&content, first_id))
    );
    upload_round(ws, &content, &needed).await;
    // The first is stored; the second is asked for again.
    assert_eq!(next_needed(ws).await, vec![second_id]);
    upload_manifest(ws, &content, second_id, second_id).await;
    upload_complete(ws).await;
    let needed = next_needed(ws).await;
    assert_eq!(
        sorted(needed.clone()),
        sorted(chunk_ids(&content, second_id))
    );
    upload_round(ws, &content, &needed).await;
    let pr = commit_publish(ws).await;
    assert_published(&pr, phase.generation);
    let _ = phase.shutdown.send(());
}

#[tokio::test]
async fn publish_refuses_a_manifest_whose_chunks_are_not_the_blob() {
    let long = noise(1, LONG);
    let id = ContentId::of(&long);
    let mut phase = publish_to_content_phase(&long).await;
    let content = phase.content.clone();
    let other = content.put(&noise(2, LONG)).unwrap();
    let ws = &mut phase.ws;
    upload_manifest(ws, &content, id, other).await;
    upload_complete(ws).await;
    let needed = next_needed(ws).await;
    upload_round(ws, &content, &needed).await;
    assert_upload_refused(&mut phase, "does not match its chunks").await;
}

#[tokio::test]
async fn publish_refuses_a_chunk_sent_as_a_manifest() {
    let long = noise(1, LONG);
    let id = ContentId::of(&long);
    let mut phase = publish_to_content_phase(&long).await;
    let content = phase.content.clone();
    let ws = &mut phase.ws;
    upload_manifest(ws, &content, id, id).await;
    upload_complete(ws).await;
    let needed = next_needed(ws).await;
    upload_manifest(ws, &content, needed[0], id).await;
    assert_upload_refused(&mut phase, "sent as a manifest").await;
}

#[tokio::test]
async fn publish_refuses_a_long_blob_sent_as_bytes() {
    let long = noise(1, LONG);
    let mut phase = publish_to_content_phase(&long).await;
    let digest = phase.needed[0].clone();
    let ws = &mut phase.ws;
    let mut offset = 0;
    for piece in long.chunks(MAX_CONTENT_CHUNK_SIZE) {
        let is_final = offset + piece.len() == long.len();
        upload_chunk(ws, &digest, offset as u64, piece, is_final).await;
        offset += piece.len();
        if offset > MAX_CONTENT_SIZE {
            break;
        }
    }
    assert_upload_refused(&mut phase, "larger than").await;
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

    let mut ws = connect_and_handshake(addr, "test/repo").await;
    let (gen1, env1_id) = open_repo(&mut ws).await;
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
    let mut ws = connect_and_handshake(addr, "test/repo").await;
    let (gen2, env2_id) = open_repo(&mut ws).await;
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
