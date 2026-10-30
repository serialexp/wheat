//! The stores read the disk synchronously, so a session hands its store work to
//! threads for blocking work instead of doing it on the async threads, where it
//! would stall every other session sharing them. These tests serve a repository on a
//! single-threaded runtime and count the store reads made on that thread.

mod support;

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use async_trait::async_trait;
use wheat_core::{ApplicationContext, ContentId, ObjectId, Repository, RepositoryEnvelopeId, Tree};
use wheat_server::registry::Registry;
use wheat_server::server::{ServerConfig, run_until};
use wheat_store::{
    BlobPage, BlobRead, ContentBlobStore, ContentStore, Durability, ImmutableObjectStore,
    LocalPublicationStore, LooseObjectStore, ObjectStoreError, RepositoryPublisher,
    SeparateRepositoryStore, StoredObject,
};

use support::{
    connect_and_handshake, edit, fetch_content, fetch_objects, fetch_objects_keeping, open_repo,
};

const REPO: &str = "test/repo";

/// Where the served stores were read: on the runtime's one thread, or elsewhere.
#[derive(Debug)]
struct Reads {
    runtime: ThreadId,
    on_runtime: AtomicUsize,
    elsewhere: AtomicUsize,
}

impl Reads {
    fn record(&self) {
        let counter = if std::thread::current().id() == self.runtime {
            &self.on_runtime
        } else {
            &self.elsewhere
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// An object store that counts where it is read.
struct CountedObjects {
    inner: LooseObjectStore,
    reads: Arc<Reads>,
}

#[async_trait]
impl ImmutableObjectStore for CountedObjects {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.record();
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.reads.record();
        self.inner.exists_many(ids).await
    }
}

/// Content blobs in memory, counting where they are read.
#[derive(Debug)]
struct CountedBlobs {
    records: Mutex<HashMap<ContentId, Vec<u8>>>,
    reads: Arc<Reads>,
}

#[async_trait]
impl ContentBlobStore for CountedBlobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()> {
        self.records.lock().unwrap().insert(id, record);
        Ok(())
    }

    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>> {
        self.reads.record();
        Ok(self.records.lock().unwrap().get(&id).cloned())
    }

    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>> {
        self.reads.record();
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
    reads: Arc<Reads>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _dir: tempfile::TempDir,
}

/// Writes a repository of a few changes, its content in blobs, with a runtime of
/// its own (content in blobs needs one to write), and returns its stores.
fn write_repository(
    dir: &std::path::Path,
    reads: &Arc<Reads>,
) -> (
    LooseObjectStore,
    ContentStore,
    LocalPublicationStore,
    RepositoryEnvelopeId,
) {
    let objects = LooseObjectStore::with_durability(dir.join("objects"), Durability::Deferred);
    let content = ContentStore::over_blobs(Arc::new(CountedBlobs {
        records: Mutex::default(),
        reads: reads.clone(),
    }));
    let heads = LocalPublicationStore::new(dir.join("repository.head"));
    let setup = tokio::runtime::Runtime::new().unwrap();
    let head = setup.block_on(async {
        let mut repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let mut view = base;
        for step in 0..8 {
            let value = format!("value {step}");
            content.put(value.as_bytes()).unwrap();
            let (_, outcome) = repository
                .create_change(
                    base,
                    view,
                    [],
                    edit(&format!("dir{}/file{step}", step % 3), value.as_bytes()),
                    ApplicationContext::new(&content),
                )
                .unwrap();
            view = outcome.resulting_view;
        }
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap()
            .envelope
    });
    (objects, content, heads, head)
}

/// Serves the repository on the current runtime, through stores that count where
/// they are read.
async fn serve(reads: Arc<Reads>) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_owned();
    let setup_reads = reads.clone();
    // Written on a thread of its own: its runtime cannot start inside this one.
    let (objects, content, heads, head) =
        std::thread::spawn(move || write_repository(&path, &setup_reads))
            .join()
            .unwrap();
    reads.on_runtime.store(0, Ordering::Relaxed);
    reads.elsewhere.store(0, Ordering::Relaxed);
    let store = SeparateRepositoryStore::new(
        CountedObjects {
            inner: objects,
            reads: reads.clone(),
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
        reads,
        _shutdown: shutdown,
        _dir: dir,
    }
}

/// Runs `test` on a single-threaded runtime, giving it the counts to record into.
fn on_one_thread<F, Fut>(test: F)
where
    F: FnOnce(Arc<Reads>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let reads = Arc::new(Reads {
        runtime: std::thread::current().id(),
        on_runtime: AtomicUsize::new(0),
        elsewhere: AtomicUsize::new(0),
    });
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(test(reads));
}

/// A store for the client side, which this test does not count.
fn client_stores(dir: &tempfile::TempDir) -> (LooseObjectStore, ContentStore) {
    (
        LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred),
        ContentStore::with_durability(dir.path().join("content"), Durability::Deferred),
    )
}

#[test]
fn a_clone_reads_the_store_off_the_async_threads() {
    on_one_thread(|reads| async move {
        let served = serve(reads).await;
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

        let reads = &served.reads;
        assert!(reads.elsewhere.load(Ordering::Relaxed) > 0);
        assert_eq!(reads.on_runtime.load(Ordering::Relaxed), 0, "{reads:?}");
    });
}

#[test]
fn an_incremental_fetch_reads_the_store_off_the_async_threads() {
    on_one_thread(|reads| async move {
        let served = serve(reads).await;
        let mut ws = connect_and_handshake(served.addr, REPO).await;
        open_repo(&mut ws).await;
        let dir = tempfile::tempdir().unwrap();
        let (objects, _) = client_stores(&dir);
        fetch_objects(&mut ws, 1, &objects, served.head, None).await;
        served.reads.on_runtime.store(0, Ordering::Relaxed);

        // `since` is the head: the server checks both envelopes and pushes nothing.
        let fetched = fetch_objects(&mut ws, 2, &objects, served.head, Some(served.head)).await;
        assert!(fetched.incremental);
        assert_eq!(
            served.reads.on_runtime.load(Ordering::Relaxed),
            0,
            "{:?}",
            served.reads
        );
    });
}
