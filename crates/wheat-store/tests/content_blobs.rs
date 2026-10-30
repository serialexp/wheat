//! Content kept in an embedder's own blob storage, through `ContentStore::over_blobs`.

use std::{
    collections::HashMap,
    io,
    ops::Range,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use wheat_core::{ContentId, VerifiedContent};
use wheat_store::{
    BlobPage, BlobRead, ContentBlobStore, ContentItem, ContentStore, ContentStoreError,
    PackedObjectStore, RepositoryStore, SeparateRepositoryStore,
};

/// Blobs in memory, as an embedder would write them, counting what is read and how
/// many writes are under way at once.
#[derive(Debug, Default)]
struct MemoryBlobs {
    records: Mutex<HashMap<ContentId, Vec<u8>>>,
    puts_under_way: AtomicUsize,
    most_puts_at_once: AtomicUsize,
    gets: AtomicUsize,
    range_reads: AtomicUsize,
    pages_listed: AtomicUsize,
    range_bytes: AtomicUsize,
    /// Page size when listing; `None` for a store that does not list.
    page: Option<usize>,
}

impl MemoryBlobs {
    fn listing(page: usize) -> Self {
        Self {
            page: Some(page),
            ..Self::default()
        }
    }

    fn record(&self, id: ContentId) -> Option<Vec<u8>> {
        self.records.lock().unwrap().get(&id).cloned()
    }

    fn set_record(&self, id: ContentId, record: Vec<u8>) {
        self.records.lock().unwrap().insert(id, record);
    }
}

#[async_trait]
impl ContentBlobStore for MemoryBlobs {
    async fn put(&self, id: ContentId, record: Vec<u8>) -> io::Result<()> {
        let under_way = self.puts_under_way.fetch_add(1, Ordering::SeqCst) + 1;
        self.most_puts_at_once
            .fetch_max(under_way, Ordering::SeqCst);
        // An upload waits on the network: the store goes on with others meanwhile.
        tokio::task::yield_now().await;
        self.set_record(id, record);
        self.puts_under_way.fetch_sub(1, Ordering::SeqCst);
        Ok(())
    }

    async fn get(&self, id: ContentId) -> io::Result<Option<Vec<u8>>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        Ok(self.record(id))
    }

    async fn get_range(&self, id: ContentId, range: Range<u64>) -> io::Result<Option<BlobRead>> {
        self.range_reads.fetch_add(1, Ordering::Relaxed);
        Ok(self.record(id).map(|record| {
            let end = (range.end as usize).min(record.len());
            let bytes = record[(range.start as usize).min(end)..end].to_vec();
            self.range_bytes.fetch_add(bytes.len(), Ordering::Relaxed);
            BlobRead {
                bytes,
                size: record.len() as u64,
            }
        }))
    }

    async fn list_page(
        &self,
        cursor: Option<String>,
        limit: usize,
    ) -> io::Result<Option<BlobPage>> {
        let Some(page) = self.page else {
            return Ok(None);
        };
        self.pages_listed.fetch_add(1, Ordering::Relaxed);
        let mut ids = self
            .records
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        ids.sort();
        let start = cursor.map_or(0, |cursor| cursor.parse().unwrap());
        let end = (start + page.min(limit)).min(ids.len());
        Ok(Some(BlobPage {
            ids: ids[start..end].to_vec(),
            next: (end < ids.len()).then(|| end.to_string()),
        }))
    }
}

fn over(blobs: &Arc<MemoryBlobs>) -> ContentStore {
    ContentStore::over_blobs(blobs.clone())
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

#[tokio::test(flavor = "multi_thread")]
async fn content_in_an_embedders_blob_store_reads_back() {
    let blobs = Arc::new(MemoryBlobs::default());
    let content = over(&blobs);
    let small = b"small".to_vec();
    let large = vec![7_u8; 256 * 1024];
    let small_id = content.put(&small).unwrap();
    let large_id = content.put(&large).unwrap();

    assert_eq!(content.get(small_id).unwrap(), small);
    assert_eq!(content.get(large_id).unwrap(), large);
    // The large content is stored compressed.
    assert!(blobs.record(large_id).unwrap().len() < large.len() / 10);
    assert_eq!(
        content.logical_length(large_id).unwrap(),
        large.len() as u64
    );
    assert!(content.contains(small_id));
    assert!(matches!(
        content.get_bounded(large_id, large.len() - 1),
        Err(ContentStoreError::TooLarge { .. })
    ));

    let missing = ContentId::of(b"never stored");
    assert!(!content.contains(missing));
    assert!(matches!(
        content.get(missing),
        Err(ContentStoreError::Missing(id)) if id == missing
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_length_reads_only_the_records_header() {
    let blobs = Arc::new(MemoryBlobs::default());
    let content = over(&blobs);
    let bytes = noise(1, 64 * 1024);
    let id = content.put(&bytes).unwrap();

    assert_eq!(content.logical_length(id).unwrap(), bytes.len() as u64);
    assert!(content.contains(id));
    assert_eq!(blobs.gets.load(Ordering::Relaxed), 0);
    assert_eq!(blobs.range_reads.load(Ordering::Relaxed), 2);
    assert!(blobs.range_bytes.load(Ordering::Relaxed) < 2 * 128);
}

#[tokio::test(flavor = "multi_thread")]
async fn content_close_to_stored_content_is_kept_as_a_delta() {
    let blobs = Arc::new(MemoryBlobs::default());
    let content = over(&blobs);
    let base = noise(2, 64 * 1024);
    let mut changed = base.clone();
    changed[1000..1010].copy_from_slice(b"0123456789");
    let base_id = content.put(&base).unwrap();
    let changed_content = VerifiedContent::new(changed.as_slice());

    content.put_with_base(changed_content, base_id).unwrap();

    let id = changed_content.id();
    assert!(blobs.record(id).unwrap().len() < 1024);
    assert_eq!(content.get(id).unwrap(), changed);
}

/// Deltas of one batch go to the blob store together, not one upload after another.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_of_deltas_is_uploaded_together() {
    let blobs = Arc::new(MemoryBlobs::default());
    let content = over(&blobs);
    let changed: Vec<(ContentId, Vec<u8>)> = (0..8)
        .map(|n| {
            let base = noise(10 + n, 16 * 1024);
            let base_id = content.put(&base).unwrap();
            let mut changed = base;
            changed[100..110].copy_from_slice(b"0123456789");
            (base_id, changed)
        })
        .collect();
    blobs.most_puts_at_once.store(0, Ordering::SeqCst);

    let items: Vec<ContentItem<'_>> = changed
        .iter()
        .map(|(base, bytes)| ContentItem::new(VerifiedContent::new(bytes.as_slice()), Some(*base)))
        .collect();
    content.put_many_with_bases(&items, 4).unwrap();

    assert!(blobs.most_puts_at_once.load(Ordering::SeqCst) > 1);
    for (_, bytes) in &changed {
        let id = ContentId::of(bytes);
        assert!(blobs.record(id).unwrap().len() < 1024, "stored as a delta");
        assert_eq!(&content.get(id).unwrap(), bytes);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_record_that_holds_other_content_is_refused() {
    let blobs = Arc::new(MemoryBlobs::default());
    let content = over(&blobs);
    let stored = content.put(b"what was stored").unwrap();
    let claimed = ContentId::of(b"what was asked for");
    blobs.set_record(claimed, blobs.record(stored).unwrap());

    assert!(matches!(
        content.get(claimed),
        Err(ContentStoreError::Corrupt { expected, actual }) if expected == claimed && actual == stored
    ));

    let mut truncated = blobs.record(stored).unwrap();
    truncated.pop();
    blobs.set_record(stored, truncated);
    assert!(matches!(
        content.get(stored),
        Err(ContentStoreError::InvalidFormat { .. })
    ));
}

/// A few IDs are checked one read each, however large the store, rather than by
/// listing it all.
#[tokio::test(flavor = "multi_thread")]
async fn a_few_ids_are_checked_one_read_each() {
    for blobs in [MemoryBlobs::listing(3), MemoryBlobs::default()] {
        let blobs = Arc::new(blobs);
        let content = over(&blobs);
        let stored = (0..8_u8)
            .map(|byte| content.put(&[byte]).unwrap())
            .collect::<Vec<_>>();
        let absent = ContentId::of(b"absent");
        let mut candidates = vec![stored[1], stored[6], absent];
        let found = content.existing_ids(&candidates).unwrap();

        candidates.pop();
        assert_eq!(found, candidates.into_iter().collect());
        assert_eq!(blobs.range_reads.load(Ordering::Relaxed), 3);
        // Listing the eight blobs takes three pages, more than the one round of reads.
        assert!(blobs.pages_listed.load(Ordering::Relaxed) <= 1);
    }
}

/// Many IDs against a store that lists them in fewer requests come from the listing.
#[tokio::test(flavor = "multi_thread")]
async fn many_ids_come_from_a_listing_when_it_is_shorter() {
    let blobs = Arc::new(MemoryBlobs::listing(3));
    let content = over(&blobs);
    let stored = (0..8_u8)
        .map(|byte| content.put(&[byte]).unwrap())
        .collect::<Vec<_>>();
    // 40 IDs take three rounds of reads; listing the eight blobs takes three pages.
    let mut candidates = stored.clone();
    candidates.extend((0..32_u8).map(|n| ContentId::of(&[n, n])));
    let found = content.existing_ids(&candidates).unwrap();

    assert_eq!(found, stored.into_iter().collect());
    assert_eq!(blobs.range_reads.load(Ordering::Relaxed), 0);
    assert_eq!(blobs.pages_listed.load(Ordering::Relaxed), 3);
}

/// An embedder keeps structure in a packed store on disk and content in its blobs.
#[tokio::test(flavor = "multi_thread")]
async fn a_repository_store_can_keep_its_content_in_blobs() {
    let root = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(root.path()).unwrap();
    let blobs = Arc::new(MemoryBlobs::default());
    let store = SeparateRepositoryStore::new(objects.clone(), over(&blobs));
    let bytes = b"published content".to_vec();
    let item = ContentItem::new(VerifiedContent::new(bytes.as_slice()), None);

    store.put_publication(&[], &[item]).await.unwrap();

    assert!(blobs.record(item.id()).is_some());
    assert!(!objects.content_store().contains(item.id()));
    assert_eq!(store.content_store().get(item.id()).unwrap(), bytes);
}
