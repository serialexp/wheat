//! The stores read and write the disk synchronously, so a session hands its store
//! work to threads for blocking work instead of doing it on the async threads, where
//! it would stall every other session sharing them. These tests serve a repository on
//! a single-threaded runtime and count the store calls made on that thread.

mod support;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree,
    ViewLabelManifestRecord,
};
use wheat_protocol::PublishResultStatus;
use wheat_server::heads::HeadStore;
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BlobPage, BlobRead, ContentBlobStore, ContentStore, Durability, ImmutableObjectStore,
    LocalPublicationStore, LooseObjectStore, ObjectStoreError, PublicationStore, RepositoryHead,
    RepositoryPublisher, SeparateRepositoryStore, StoredObject,
};

use support::{
    Upload, connect_and_handshake, edit, fetch_content, fetch_objects, fetch_objects_keeping,
    open_repo, request_publish, upload_content_and_commit, upload_objects,
};

const REPO: &str = "test/repo";

/// Where the served stores were read or written: on the runtime's one thread, or
/// elsewhere.
#[derive(Debug)]
struct Uses {
    runtime: ThreadId,
    on_runtime: AtomicUsize,
    elsewhere: AtomicUsize,
}

impl Uses {
    fn record(&self) {
        let counter = if std::thread::current().id() == self.runtime {
            &self.on_runtime
        } else {
            &self.elsewhere
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// An object store that counts where it is used.
struct CountedObjects {
    inner: LooseObjectStore,
    uses: Arc<Uses>,
}

#[async_trait]
impl ImmutableObjectStore for CountedObjects {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.uses.record();
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.uses.record();
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.uses.record();
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.uses.record();
        self.inner.exists_many(ids).await
    }
}

/// Content blobs in memory, counting where they are used.
#[derive(Debug)]
struct CountedBlobs {
    records: Mutex<HashMap<ContentId, Vec<u8>>>,
    uses: Arc<Uses>,
}

#[async_trait]
impl ContentBlobStore for CountedBlobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()> {
        self.uses.record();
        self.records.lock().unwrap().insert(id, record);
        Ok(())
    }

    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>> {
        self.uses.record();
        Ok(self.records.lock().unwrap().get(&id).cloned())
    }

    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>> {
        self.uses.record();
        Ok(self.records.lock().unwrap().get(&id).map(|record| {
            let end = (range.end as usize).min(record.len());
            BlobRead {
                bytes: record[(range.start as usize).min(end)..end].to_vec(),
                size: record.len() as u64,
            }
        }))
    }

    async fn list_page(
        &self,
        _cursor: Option<String>,
        _limit: usize,
    ) -> io::Result<Option<BlobPage>> {
        Ok(None)
    }
}

struct Served {
    addr: SocketAddr,
    head: RepositoryEnvelopeId,
    client: Client,
    uses: Arc<Uses>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

/// A client holding the served repository and one change more.
struct Client {
    objects: LooseObjectStore,
    content: ContentStore,
    /// The envelope of the client's repository, which the server lacks.
    proposed: RepositoryEnvelopeId,
    _dir: tempfile::TempDir,
}

/// Writes a repository of a few changes, its content in blobs, with a runtime of
/// its own (content in blobs needs one to write), and returns its stores, with a
/// client that has made one change more.
fn write_repository(
    dir: &std::path::Path,
    uses: &Arc<Uses>,
) -> (
    LooseObjectStore,
    ContentStore,
    LocalPublicationStore,
    RepositoryEnvelopeId,
    Client,
) {
    let objects = LooseObjectStore::with_durability(dir.join("objects"), Durability::Deferred);
    let content = ContentStore::over_blobs(Arc::new(CountedBlobs {
        records: Mutex::default(),
        uses: uses.clone(),
    }));
    let heads = LocalPublicationStore::new(dir.join("repository.head"));
    let client_dir = tempfile::tempdir().unwrap();
    let (client_objects, client_content) = client_stores(&client_dir);
    let client_heads = LocalPublicationStore::new(client_dir.path().join("repository.head"));
    let setup = tokio::runtime::Runtime::new().unwrap();
    let (head, proposed) = setup.block_on(async {
        let mut repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let mut view = base;
        let mut head = None;
        // The server has the first eight changes; the client also the ninth.
        for step in 0..9 {
            let value = format!("value {step}");
            if step < 8 {
                content.put(value.as_bytes()).unwrap();
            }
            client_content.put(value.as_bytes()).unwrap();
            let (_, outcome) = repository
                .create_change(
                    base,
                    view,
                    [],
                    edit(&format!("dir{}/file{step}", step % 3), value.as_bytes()),
                    ApplicationContext::new(&client_content),
                )
                .unwrap();
            view = outcome.resulting_view;
            if step == 7 {
                let published = RepositoryPublisher::new(&objects, &content, &heads)
                    .initialize(&repository)
                    .await
                    .unwrap();
                head = Some(published.envelope);
            }
        }
        let publisher = RepositoryPublisher::new(&client_objects, &client_content, &client_heads);
        let closure = publisher.prepare(&repository).await.unwrap();
        let labels = ViewLabelManifestRecord {
            labels: Default::default(),
        };
        let proposed = publisher
            .prepare_envelope(closure.state.id, &labels)
            .await
            .unwrap();
        (head.unwrap(), proposed)
    });
    let client = Client {
        objects: client_objects,
        content: client_content,
        proposed,
        _dir: client_dir,
    };
    (objects, content, heads, head, client)
}

/// Serves the repository on the current runtime, through stores that count where
/// they are used.
async fn serve(uses: Arc<Uses>) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let setup_uses = uses.clone();
    // Written on a thread of its own: its runtime cannot start inside this one.
    let (objects, content, heads, head, client) =
        std::thread::spawn(move || write_repository(&path, &setup_uses))
            .join()
            .unwrap();
    uses.on_runtime.store(0, Ordering::Relaxed);
    uses.elsewhere.store(0, Ordering::Relaxed);
    let store = SeparateRepositoryStore::new(
        CountedObjects {
            inner: objects,
            uses: uses.clone(),
        },
        content,
    );
    let registry = Registry::single_with_stores(REPO.to_string(), Box::new(store), heads);
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = tokio::sync::oneshot::channel();
    let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();
    tokio::spawn(async move {
        run_until(any_port, registry, ServerConfig::new(), ready, shutdown_rx)
            .await
            .unwrap();
    });
    Served {
        addr: ready_rx.await.unwrap(),
        head,
        client,
        uses,
        _shutdown: shutdown,
        _dir: dir,
    }
}

/// Runs `test` on a single-threaded runtime, giving it the counts to record into.
fn on_one_thread<F, Fut>(test: F)
where
    F: FnOnce(Arc<Uses>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let uses = Arc::new(Uses {
        runtime: std::thread::current().id(),
        on_runtime: AtomicUsize::new(0),
        elsewhere: AtomicUsize::new(0),
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test(uses));
}

/// Stores for the client side, which these tests do not count.
fn client_stores(dir: &tempfile::TempDir) -> (LooseObjectStore, ContentStore) {
    (
        LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred),
        ContentStore::with_durability(dir.path().join("content"), Durability::Deferred),
    )
}

#[test]
fn a_clone_reads_the_store_off_the_async_threads() {
    on_one_thread(|uses| async move {
        let served = serve(uses).await;
        let mut ws = connect_and_handshake(served.addr, REPO).await;
        open_repo(&mut ws).await;
        let dir = tempfile::tempdir().unwrap();
        let (objects, content) = client_stores(&dir);

        // Dropping every other pushed object makes the client ask for them, so
        // the wants are answered too.
        let fetched =
            fetch_objects_keeping(&mut ws, 1, &objects, served.head, None, |n| n % 2 == 0).await;
        assert!(fetched.wants > 0, "{fetched:?}");
        let blobs = fetch_content(&mut ws, 2, &objects, &content, served.head).await;
        assert!(blobs > 0);

        let uses = &served.uses;
        assert!(uses.elsewhere.load(Ordering::Relaxed) > 0);
        assert_eq!(uses.on_runtime.load(Ordering::Relaxed), 0, "{uses:?}");
    });
}

#[test]
fn an_incremental_fetch_reads_the_store_off_the_async_threads() {
    on_one_thread(|uses| async move {
        let served = serve(uses).await;
        let mut ws = connect_and_handshake(served.addr, REPO).await;
        open_repo(&mut ws).await;
        let dir = tempfile::tempdir().unwrap();
        let (objects, _) = client_stores(&dir);
        fetch_objects(&mut ws, 1, &objects, served.head, None).await;
        served.uses.on_runtime.store(0, Ordering::Relaxed);

        // `since` is the head: the server checks both envelopes and pushes nothing.
        let fetched = fetch_objects(&mut ws, 2, &objects, served.head, Some(served.head)).await;
        assert!(fetched.incremental);
        assert_eq!(
            served.uses.on_runtime.load(Ordering::Relaxed),
            0,
            "{:?}",
            served.uses
        );
    });
}

#[test]
fn a_publish_uses_the_store_off_the_async_threads() {
    on_one_thread(|uses| async move {
        let served = serve(uses).await;
        let mut ws = connect_and_handshake(served.addr, REPO).await;
        let head = open_repo(&mut ws).await;
        let client = &served.client;

        // Nothing is pushed, so the server asks for every object and blob, staging
        // each round, and then joins, validates and stores the publish.
        request_publish(&mut ws, 1, client.proposed, head, false).await;
        let rounds = upload_objects(&mut ws, 1, &client.objects, Upload::Whole).await;
        assert!(!rounds.is_empty());
        let (result, asked) = upload_content_and_commit(&mut ws, 1, &client.content).await;
        assert!(
            matches!(result.status, PublishResultStatus::PublishStatusOk(_)),
            "{}",
            result.message
        );
        assert!(!asked.is_empty());

        let uses = &served.uses;
        assert!(uses.elsewhere.load(Ordering::Relaxed) > 0);
        assert_eq!(uses.on_runtime.load(Ordering::Relaxed), 0, "{uses:?}");
    });
}

/// A head read waiting for the head's lock, held here by another process, say, waits
/// on a thread for blocking work: the runtime goes on with its other tasks.
#[test]
fn a_head_read_waits_for_its_lock_off_the_async_threads() {
    let dir = tempfile::tempdir().unwrap();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let head = RepositoryHead {
        generation: 1,
        envelope: RepositoryEnvelopeId::from_digest([7; 32]),
    };
    heads.initialize(head).unwrap();

    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(dir.path().join("repository.lock"))
        .unwrap();
    lock.lock().unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let (release, release_rx) = std::sync::mpsc::channel::<()>();
    let holder = {
        let released = released.clone();
        std::thread::spawn(move || {
            // Held until the test says, or long enough that a runtime the read
            // blocked has visibly stood still; then the test fails instead of hanging.
            let _ = release_rx.recv_timeout(Duration::from_secs(10));
            released.store(true, Ordering::SeqCst);
            lock.unlock().unwrap();
        })
    };

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let read = tokio::spawn(async move { HeadStore::head(&heads).await });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(
                !released.load(Ordering::SeqCst),
                "the runtime stood still until the lock was released"
            );
            release.send(()).unwrap();
            assert_eq!(read.await.unwrap().unwrap(), head);
        });
    holder.join().unwrap();
}
