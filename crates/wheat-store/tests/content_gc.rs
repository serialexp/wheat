//! Content GC over an embedder's blob store: marks from each network's heads, merged
//! across hosts, a sweep that keeps what marked records refer to and reclaims the
//! rest, failing closed.

use crate::support;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ContentId, Repository, RepositoryEnvelopeId, Tree, VerifiedContent,
};
use wheat_store::{
    BlobPage, BlobRead, ContentBlobStore, ContentInventory, ContentMarks, ContentStore, GcError,
    LocalPublicationStore, PackedObjectStore, PublicationStore, RecordReferences,
    RepositoryPublisher, RepositoryReader, record_info, sweep,
};

/// Records by ID, listed two to a page so sweeping follows the cursor.
#[derive(Debug, Default)]
struct Blobs {
    records: Mutex<BTreeMap<ContentId, Vec<u8>>>,
}

#[async_trait]
impl ContentBlobStore for Blobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> std::io::Result<()> {
        self.records.lock().unwrap().insert(id, record);
        Ok(())
    }

    async fn get(&self, id: ContentId) -> std::io::Result<Option<Vec<u8>>> {
        Ok(self.records.lock().unwrap().get(&id).cloned())
    }

    async fn get_range(
        &self,
        id: ContentId,
        range: std::ops::Range<u64>,
    ) -> std::io::Result<Option<BlobRead>> {
        Ok(self.records.lock().unwrap().get(&id).map(|record| {
            let end = (range.end as usize).min(record.len());
            let start = (range.start as usize).min(end);
            BlobRead {
                bytes: record[start..end].to_vec(),
                size: record.len() as u64,
            }
        }))
    }

    async fn list_page(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> std::io::Result<Option<BlobPage>> {
        let records = self.records.lock().unwrap();
        let after = cursor.map(|cursor| ContentId::from_digest(hex(&cursor)));
        let mut ids: Vec<ContentId> = records
            .keys()
            .filter(|id| after.is_none_or(|after| **id > after))
            .copied()
            .take(limit.min(2))
            .collect();
        ids.truncate(2);
        let next =
            (ids.len() == 2 && records.keys().any(|id| id > ids.last().unwrap())).then(|| {
                ids.last()
                    .unwrap()
                    .as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect()
            });
        Ok(Some(BlobPage { ids, next }))
    }
}

fn hex(text: &str) -> [u8; 32] {
    let mut out = [0; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

/// The embedder's rows: each record's references as `record_info` gave them, and
/// what was reclaimed.
struct Inventory {
    blobs: Arc<Blobs>,
    reclaimed: Mutex<BTreeSet<ContentId>>,
    /// An ID the embedder has lost its row for.
    forgotten: Option<ContentId>,
    /// An ID named since the mark began, which it keeps.
    named: Option<ContentId>,
    /// The longest batch it was asked to reclaim.
    largest_reclaim: Mutex<usize>,
}

#[async_trait]
impl ContentInventory for Inventory {
    async fn references(
        &self,
        ids: &[ContentId],
    ) -> std::io::Result<Vec<(ContentId, RecordReferences)>> {
        let records = self.blobs.records.lock().unwrap();
        Ok(ids
            .iter()
            .filter(|id| Some(**id) != self.forgotten)
            .filter_map(|id| {
                let record = records.get(id)?;
                Some((*id, record_info(*id, record).unwrap().references))
            })
            .collect())
    }

    async fn reclaim(&self, ids: &[ContentId]) -> std::io::Result<Vec<ContentId>> {
        let mut largest = self.largest_reclaim.lock().unwrap();
        *largest = (*largest).max(ids.len());
        let reclaimed: Vec<_> = ids
            .iter()
            .copied()
            .filter(|id| Some(*id) != self.named)
            .collect();
        let mut records = self.blobs.records.lock().unwrap();
        for id in &reclaimed {
            records.remove(id);
        }
        self.reclaimed.lock().unwrap().extend(&reclaimed);
        Ok(reclaimed)
    }
}

fn inventory(blobs: &Arc<Blobs>) -> Inventory {
    Inventory {
        blobs: Arc::clone(blobs),
        reclaimed: Mutex::default(),
        forgotten: None,
        named: None,
        largest_reclaim: Mutex::default(),
    }
}

/// When the test's collection began: before anything was marked.
fn started() -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_millis(1_762_400_000_123)
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

/// A network: its own pool, and a head over the shared content.
struct Network {
    _dir: tempfile::TempDir,
    objects: PackedObjectStore,
    head: RepositoryEnvelopeId,
}

/// A repository writing `files` in one change, published into a pool of its own.
async fn network(content: &ContentStore, files: &[(&str, ContentId)]) -> Network {
    let dir = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(dir.path().join("storage")).unwrap();
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let mut repo = Repository::new(Tree::new());
    let base = repo.base_view().id;
    let mut view = base;
    for (path, id) in files {
        let bytes = content.get(*id).unwrap();
        let (_, outcome) = repo
            .create_change(
                base,
                view,
                [],
                support::exact(path, None, &bytes),
                ApplicationContext::new(content),
            )
            .unwrap();
        view = outcome.resulting_view;
    }
    RepositoryPublisher::new(&objects, content, &heads)
        .initialize(&repo)
        .await
        .unwrap();
    Network {
        head: heads.read().unwrap().envelope,
        _dir: dir,
        objects,
    }
}

/// Content for two networks over one blob store: shared, each network's own, a delta
/// whose base no head names, a chunked blob, and content no head names at all.
struct Instance {
    blobs: Arc<Blobs>,
    content: ContentStore,
    a: Network,
    b: Network,
    kept: BTreeSet<ContentId>,
    dead: BTreeSet<ContentId>,
}

async fn instance() -> Instance {
    let blobs = Arc::new(Blobs::default());
    let content = ContentStore::over_blobs(blobs.clone());
    let shared = content.put(b"shared by both networks").unwrap();
    let only_a = content.put(b"only network a").unwrap();
    let only_b = content.put(b"only network b").unwrap();
    let base = noise(1, 64 * 1024);
    let mut changed = base.clone();
    changed[100..110].copy_from_slice(b"0123456789");
    let delta_base = content.put(&base).unwrap();
    let delta = VerifiedContent::new(changed.as_slice());
    content.put_with_base(delta, delta_base).unwrap();
    let delta = delta.id();
    let long = noise(2, 20 << 20);
    let mut writer = content.writer();
    writer.write_all(&long).unwrap();
    let chunked = writer.finish(None).unwrap();
    let dead: BTreeSet<_> = [
        content.put(b"named by no head").unwrap(),
        content.put(b"nor this").unwrap(),
    ]
    .into();
    let a = network(
        &content,
        &[
            ("shared", shared),
            ("a", only_a),
            ("delta", delta),
            ("long", chunked),
        ],
    )
    .await;
    let b = network(&content, &[("shared", shared), ("b", only_b)]).await;
    let kept = blobs
        .records
        .lock()
        .unwrap()
        .keys()
        .filter(|id| !dead.contains(id))
        .copied()
        .collect();
    assert!(matches!(
        record_info(delta, &blobs.records.lock().unwrap()[&delta]).unwrap().references,
        RecordReferences::Delta { base } if base == delta_base
    ));
    Instance {
        blobs,
        content,
        a,
        b,
        kept,
        dead,
    }
}

/// One host's finished marks of `networks`.
async fn marked(path: &Path, content: &ContentStore, networks: &[&Network]) -> std::path::PathBuf {
    let mut marks = ContentMarks::create(path, started()).unwrap();
    for network in networks {
        let reader = RepositoryReader::new(&network.objects, content);
        marks.mark_heads(&reader, [network.head]).await.unwrap();
    }
    marks.finish().unwrap()
}

fn stored(blobs: &Blobs) -> BTreeSet<ContentId> {
    blobs.records.lock().unwrap().keys().copied().collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sweep_reclaims_only_what_no_head_needs() {
    let instance = instance().await;
    let dir = tempfile::tempdir().unwrap();
    // Two hosts, one network each, merged by the worker.
    let host_a = marked(
        &dir.path().join("a.marks"),
        &instance.content,
        &[&instance.a],
    )
    .await;
    let host_b = marked(
        &dir.path().join("b.marks"),
        &instance.content,
        &[&instance.b],
    )
    .await;
    let mut marks = ContentMarks::merge(
        &dir.path().join("all.marks"),
        started(),
        &[&host_a, &host_b],
    )
    .unwrap();
    assert_eq!(marks.started(), started());
    let blobs: Arc<dyn ContentBlobStore> = instance.blobs.clone();
    // One dead record was named since the mark began: the inventory keeps it.
    let named = *instance.dead.iter().next().unwrap();
    let inventory = Inventory {
        named: Some(named),
        ..inventory(&instance.blobs)
    };
    let counts = sweep(&blobs, &mut marks, &inventory).await.unwrap();

    let mut reclaimed = instance.dead.clone();
    reclaimed.remove(&named);
    let mut kept = instance.kept.clone();
    kept.insert(named);
    assert_eq!(*inventory.reclaimed.lock().unwrap(), reclaimed);
    assert_eq!(stored(&instance.blobs), kept);
    assert_eq!(counts.reclaimed, reclaimed.len() as u64);
    assert_eq!(counts.kept, 1); // A listed page's unmarked IDs go to the inventory at once: this store lists
    // two IDs a page.
    assert!(
        (1..=counts.pages).contains(&counts.reclaim_calls),
        "{counts:?}"
    );
    assert_eq!(counts.pages, counts.listed.div_ceil(2), "{counts:?}");
    // Never more than a page; the bench case counts the calls at scale.
    assert!((1..=2).contains(&*inventory.largest_reclaim.lock().unwrap()));
    assert_eq!(
        counts.listed,
        (instance.kept.len() + instance.dead.len()) as u64
    );
    // The delta's base and the chunked blob's chunks were found by reference.
    assert!(counts.referenced > 1, "{counts:?}");
    // Every head still reads whole.
    for network in [&instance.a, &instance.b] {
        let reader = RepositoryReader::new(&network.objects, &instance.content);
        let envelope = reader.envelope(network.head).await.unwrap();
        let state = reader.state(envelope.state).await.unwrap();
        let mut cursor = wheat_store::ContentSetStore::new(&network.objects)
            .cursor(state.required_content)
            .await
            .unwrap();
        while let Some(id) = cursor.next().await.unwrap() {
            instance.content.get_verified(id).unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn heads_sharing_content_sets_are_walked_once() {
    let instance = instance().await;
    let dir = tempfile::tempdir().unwrap();
    let mut marks = ContentMarks::create(&dir.path().join("marks"), started()).unwrap();
    let reader = RepositoryReader::new(&instance.a.objects, &instance.content);
    let first = marks.mark_heads(&reader, [instance.a.head]).await.unwrap();
    assert_eq!(first.heads, 1);
    assert!(first.nodes_read > 0 && first.marked > 0, "{first:?}");
    let again = marks.mark_heads(&reader, [instance.a.head]).await.unwrap();
    assert_eq!(again.nodes_read, 0, "{again:?}");
    assert_eq!(again.nodes_skipped, 1, "{again:?}");
    assert_eq!(again.marked, 0);
    assert_eq!(marks.len().unwrap(), first.marked);
}

#[tokio::test(flavor = "multi_thread")]
async fn collection_fails_closed() {
    let instance = instance().await;
    let dir = tempfile::tempdir().unwrap();
    let blobs: Arc<dyn ContentBlobStore> = instance.blobs.clone();
    let before = stored(&instance.blobs);

    // A head that cannot be read stops marking.
    let mut marks = ContentMarks::create(&dir.path().join("missing"), started()).unwrap();
    let reader = RepositoryReader::new(&instance.a.objects, &instance.content);
    let unknown = instance.b.head;
    assert!(marks.mark_heads(&reader, [unknown]).await.is_err());

    // An unfinished mark file is neither opened, merged nor swept against.
    let unfinished = dir.path().join("unfinished");
    let mut open = ContentMarks::create(&unfinished, started()).unwrap();
    open.mark_heads(&reader, [instance.a.head]).await.unwrap();
    assert!(ContentMarks::open(&unfinished).is_err());
    assert!(ContentMarks::merge(&dir.path().join("merged"), started(), &[&unfinished]).is_err());
    assert!(matches!(
        sweep(&blobs, &mut open, &inventory(&instance.blobs)).await,
        Err(GcError::File(_))
    ));
    drop(open);

    // A marked record the inventory cannot answer for stops the sweep before any
    // reclaim.
    let path = marked(
        &dir.path().join("all"),
        &instance.content,
        &[&instance.a, &instance.b],
    )
    .await;
    // A file marked for an earlier collection is not merged into a later one.
    let later = started() + std::time::Duration::from_secs(60);
    assert!(matches!(
        ContentMarks::merge(&dir.path().join("later"), later, &[&path]),
        Err(GcError::File(_))
    ));
    let mut marks = ContentMarks::open(&path).unwrap();
    assert_eq!(marks.started(), started());
    let forgetful = Inventory {
        forgotten: instance.kept.iter().next().copied(),
        ..inventory(&instance.blobs)
    };
    assert!(matches!(
        sweep(&blobs, &mut marks, &forgetful).await,
        Err(GcError::Unknown(_))
    ));
    assert!(forgetful.reclaimed.lock().unwrap().is_empty());
    assert_eq!(stored(&instance.blobs), before);
}
