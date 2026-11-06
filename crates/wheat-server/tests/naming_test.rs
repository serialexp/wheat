//! Content naming: a network collected by age is told what each publish relies on
//! without writing it, before the publish joins the commit, and told again as the
//! batch commits when that was long ago. A naming or a store write refused while
//! content is being reclaimed asks the client to publish again.

use crate::support;

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ContentId, Repository, RepositoryEnvelopeId, Tree, VerifiedContent,
    ViewLabelManifestRecord,
};
use wheat_protocol::*;
use wheat_server::naming::ContentNaming;
use wheat_server::network::Network;
use wheat_server::registry::{Registry, RepositoryEntry};
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BlobRead, ContentBlobStore, ContentStore, ContentStoreError, Durability, LocalPublicationStore,
    LooseObjectStore, PublicationStore, RepositoryPublisher, SeparateRepositoryStore,
};

use support::{
    Upload, connect_and_handshake, content_batch, edits, open_repo, recv_msg, request_publish,
    rewrites, send_msg, upload_objects,
};

const REPO: &str = "test/repo";

/// Content blobs in memory, refusing every write while `refuse` is set.
#[derive(Debug, Default)]
struct Blobs {
    records: Mutex<HashMap<ContentId, Vec<u8>>>,
    refuse: AtomicBool,
}

#[async_trait]
impl ContentBlobStore for Blobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()> {
        if self.refuse.load(Ordering::Relaxed) {
            return Err(io::Error::other("the row is being reclaimed"));
        }
        self.records.lock().unwrap().insert(id, record);
        Ok(())
    }

    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>> {
        Ok(self.records.lock().unwrap().get(&id).cloned())
    }

    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>> {
        Ok(self.records.lock().unwrap().get(&id).map(|record| {
            let end = (range.end as usize).min(record.len());
            BlobRead {
                bytes: record[(range.start as usize).min(end)..end].to_vec(),
                size: record.len() as u64,
            }
        }))
    }
}

/// Records each naming; refuses them all when `fail`.
struct Recording {
    renew_after: Duration,
    fail: bool,
    calls: Mutex<Vec<Vec<ContentId>>>,
}

#[async_trait]
impl ContentNaming for Recording {
    fn renew_after(&self) -> Duration {
        self.renew_after
    }

    async fn named(&self, ids: &[ContentId]) -> io::Result<()> {
        self.calls.lock().unwrap().push(ids.to_vec());
        if self.fail {
            return Err(io::Error::other("content was reclaimed"));
        }
        Ok(())
    }
}

fn recording(renew_after: Duration, fail: bool) -> Arc<Recording> {
    Arc::new(Recording {
        renew_after,
        fail,
        calls: Mutex::default(),
    })
}

/// `len` bytes that don't compress, from `seed`.
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// A served repository, a client's publish to it, and the content the publish's
/// naming is about.
struct Served {
    addr: SocketAddr,
    head: (u64, RepositoryEnvelopeId),
    heads: LocalPublicationStore,
    client: ContentStore,
    client_objects: LooseObjectStore,
    proposed: RepositoryEnvelopeId,
    /// Stored on the server outside its head, and added by the publish.
    orphan: ContentId,
    /// `a.txt` before and after the publish: a delta base and an upload.
    small: ContentId,
    small_changed: ContentId,
    /// `big.bin`, chunked, before and after.
    big: ContentId,
    big_changed: ContentId,
    blobs: Arc<Blobs>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// A server whose head has `a.txt` and a chunked `big.bin` and that holds `x.txt`'s
/// content outside it, telling `naming`; and a client that changes both files and
/// adds `x.txt`.
async fn serve(naming: Arc<Recording>) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let client_dir = tempfile::tempdir().unwrap();
    let objects =
        LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred);
    let blobs = Arc::new(Blobs::default());
    let content = ContentStore::over_blobs(Arc::clone(&blobs) as Arc<dyn ContentBlobStore>);
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let client_objects =
        LooseObjectStore::with_durability(client_dir.path().join("objects"), Durability::Deferred);
    let client =
        ContentStore::with_durability(client_dir.path().join("content"), Durability::Deferred);
    let client_heads = LocalPublicationStore::new(client_dir.path().join("repository.head"));

    let small = noise(1, 64 * 1024);
    let mut small_changed = small.clone();
    small_changed[1000..1010].copy_from_slice(b"0123456789");
    let big = noise(2, 17 << 20);
    let mut big_changed = big.clone();
    big_changed[8 << 20..(8 << 20) + 10].copy_from_slice(b"0123456789");
    let orphan = noise(3, 4096);
    for bytes in [&small, &small_changed, &big, &big_changed, &orphan] {
        client.put(bytes).unwrap();
    }
    content.put(&small).unwrap();
    content.put(&big).unwrap();
    content.put(&orphan).unwrap();

    let mut repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let (_, first) = repository
        .create_change(
            base,
            base,
            [],
            edits([("a.txt", small.as_slice()), ("big.bin", big.as_slice())]),
            ApplicationContext::new(&client),
        )
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    let head = heads.read().unwrap();
    repository
        .create_change(
            base,
            first.resulting_view,
            [],
            rewrites([
                ("a.txt", Some(small.as_slice()), small_changed.as_slice()),
                ("big.bin", Some(big.as_slice()), big_changed.as_slice()),
                ("x.txt", None, orphan.as_slice()),
            ]),
            ApplicationContext::new(&client),
        )
        .unwrap();
    let publisher = RepositoryPublisher::new(&client_objects, &client, &client_heads);
    let closure = publisher.prepare(&repository).await.unwrap();
    let labels = ViewLabelManifestRecord {
        labels: Default::default(),
    };
    let proposed = publisher
        .prepare_envelope(closure.state.id, &labels)
        .await
        .unwrap();

    let store = SeparateRepositoryStore::new(objects, content);
    let network = Network::with_naming(Box::new(store), naming);
    let registry = Registry::single_entry(
        REPO.to_string(),
        RepositoryEntry::in_network(network, heads.clone()),
    );
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(any_port, registry, ServerConfig::new(), ready, shutdown_rx)
            .await
            .unwrap();
    });
    let id = |bytes: &[u8]| VerifiedContent::new(bytes).id();
    Served {
        addr: ready_rx.await.unwrap(),
        head: (head.generation, head.envelope),
        heads,
        client_objects,
        proposed,
        orphan: id(&orphan),
        small: id(&small),
        small_changed: id(&small_changed),
        big: id(&big),
        big_changed: id(&big_changed),
        blobs,
        client,
        _shutdown: shutdown,
        _dirs: (dir, client_dir),
    }
}

/// Answers the content phase, sending a long blob as its manifest, and commits; or
/// the result the server answers with during the upload.
async fn upload_and_commit(
    ws: &mut support::Ws,
    op_id: u64,
    content: &ContentStore,
) -> PublishResultOutput {
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::ContentNeeded(needed) => {
                for digest in needed.content_ids {
                    let id = ContentId::from_digest(digest.bytes.as_slice().try_into().unwrap());
                    match content.get_bounded(id, MAX_CONTENT_SIZE) {
                        Ok(data) => send_msg(ws, &content_batch(op_id, [(id, data)])).await,
                        Err(ContentStoreError::TooLarge { .. }) => {
                            let manifest = content.chunk_manifest(id).unwrap().unwrap();
                            let manifest =
                                wheat_core::encode_content_chunk_manifest(&manifest).unwrap();
                            send_msg(ws, &content_manifest(op_id, digest, manifest)).await;
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
                send_msg(
                    ws,
                    &WheatProtocolMessage::ContentUploadComplete(ContentUploadCompleteOutput {
                        message_type: 0x46,
                        op_id,
                    }),
                )
                .await;
            }
            WheatProtocolMessage::ContentComplete(_) => break,
            WheatProtocolMessage::PublishResult(result) => return result,
            other => panic!("expected ContentNeeded or ContentComplete, got {other:?}"),
        }
    }
    send_msg(
        ws,
        &WheatProtocolMessage::PublishCommit(PublishCommitOutput {
            message_type: 0x47,
            op_id,
        }),
    )
    .await;
    loop {
        match recv_msg(ws).await {
            WheatProtocolMessage::PublishProgress(_) => continue,
            WheatProtocolMessage::PublishResult(result) => return result,
            other => panic!("expected PublishResult, got {other:?}"),
        }
    }
}

async fn publish(served: &Served) -> PublishResultOutput {
    let mut ws = connect_and_handshake(served.addr, REPO).await;
    let head = open_repo(&mut ws).await;
    assert_eq!(head, served.head);
    request_publish(&mut ws, 1, served.proposed, head, false).await;
    upload_objects(&mut ws, 1, &served.client_objects, Upload::Whole).await;
    upload_and_commit(&mut ws, 1, &served.client).await
}

/// The chunks a chunked blob of `content` lists.
fn chunks(content: &ContentStore, id: ContentId) -> HashSet<ContentId> {
    let manifest = content.chunk_manifest(id).unwrap().unwrap();
    let wheat_core::ContentManifestEntries::Chunks(chunks) = manifest.entries else {
        panic!("a 17 MiB blob lists its chunks");
    };
    chunks.into_iter().map(|chunk| chunk.id).collect()
}

fn published(result: &PublishResultOutput) -> bool {
    matches!(result.status, PublishResultStatus::PublishStatusOk(_))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_names_what_it_relies_on_and_its_batch_names_it_again() {
    // Renewing at once: the batch always names again.
    let naming = recording(Duration::ZERO, false);
    let served = serve(Arc::clone(&naming)).await;
    let result = publish(&served).await;
    assert!(published(&result), "{}", result.message);
    assert_eq!(served.heads.read().unwrap().envelope, served.proposed);

    let calls = naming.calls.lock().unwrap().clone();
    let [first, again] = calls.as_slice() else {
        panic!("expected a naming and a renewal, got {calls:?}");
    };
    let first: HashSet<_> = first.iter().copied().collect();
    // The content found stored, the uploaded manifest and every chunk it lists, the
    // ones stored already with them. Not the old long blob, a chunked blob being no
    // delta's base, nor its chunks. Nor `a.txt`'s old content: a publish takes delta
    // bases from the base view's tree, which a change leaves as it is, so nothing it
    // uploads is stored as a delta (TODO.md).
    let new_chunks = chunks(&served.client, served.big_changed);
    let old_chunks = chunks(&served.client, served.big);
    assert!(
        new_chunks.intersection(&old_chunks).count() > 0,
        "the long blobs share chunks"
    );
    let mut expected: HashSet<_> = [served.orphan, served.big_changed].into();
    expected.extend(new_chunks.iter().copied());
    let name = |id: &ContentId| {
        [
            (served.orphan, "orphan"),
            (served.small, "small"),
            (served.small_changed, "small_changed"),
            (served.big, "big"),
            (served.big_changed, "big_changed"),
        ]
        .iter()
        .find(|(known, _)| known == id)
        .map_or_else(
            || {
                if new_chunks.contains(id) {
                    "a new chunk".to_owned()
                } else if old_chunks.contains(id) {
                    "an old chunk".to_owned()
                } else {
                    id.to_string()
                }
            },
            |(_, name)| (*name).to_owned(),
        )
    };
    let missing: Vec<_> = expected.difference(&first).map(name).collect();
    let extra: Vec<_> = first.difference(&expected).map(name).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing {missing:?}, extra {extra:?}"
    );
    assert!(!first.contains(&served.big));
    // As it commits, the batch names that again with what it uploaded.
    let again: HashSet<_> = again.iter().copied().collect();
    expected.insert(served.small_changed);
    assert_eq!(again, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recent_naming_is_not_repeated_as_the_batch_commits() {
    let naming = recording(Duration::from_secs(3600), false);
    let served = serve(Arc::clone(&naming)).await;
    let result = publish(&served).await;
    assert!(published(&result), "{}", result.message);
    assert_eq!(naming.calls.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_publish_whose_naming_fails_is_refused_to_be_sent_again() {
    let naming = recording(Duration::ZERO, true);
    let served = serve(Arc::clone(&naming)).await;
    let result = publish(&served).await;
    assert!(
        matches!(result.status, PublishResultStatus::PublishStatusError(_)),
        "{:?}",
        result.status
    );
    assert!(
        result.message.contains("content was reclaimed")
            && result.message.contains("publish again"),
        "{}",
        result.message
    );
    let head = served.heads.read().unwrap();
    assert_eq!((head.generation, head.envelope), served.head);
    assert_eq!(naming.calls.lock().unwrap().len(), 1);
}

/// The manifest of the long blob is stored while the publish uploads, before any
/// commit: a refused write there is answered, and nothing of the publish is kept.
#[tokio::test(flavor = "multi_thread")]
async fn a_publish_whose_upload_cannot_be_stored_is_refused_to_be_sent_again() {
    let served = serve(recording(Duration::from_secs(3600), false)).await;
    served.blobs.refuse.store(true, Ordering::Relaxed);
    let result = publish(&served).await;
    assert!(
        matches!(result.status, PublishResultStatus::PublishStatusError(_)),
        "{:?}: {}",
        result.status,
        result.message
    );
    assert!(
        result.message.contains("the row is being reclaimed")
            && result.message.contains("publish again"),
        "{}",
        result.message
    );
    let head = served.heads.read().unwrap();
    assert_eq!((head.generation, head.envelope), served.head);

    // Sent again once the store takes writes, it is published.
    served.blobs.refuse.store(false, Ordering::Relaxed);
    let result = publish(&served).await;
    assert!(published(&result), "{}", result.message);
    assert_eq!(served.heads.read().unwrap().envelope, served.proposed);
}
