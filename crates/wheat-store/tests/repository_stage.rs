use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use wheat_core::{ContentId, DirectoryNode, ObjectId, VerifiedContent};
use wheat_store::*;

fn object() -> (ObjectId, Vec<u8>) {
    let bytes = DirectoryNode::default().canonical_bytes();
    (ObjectId::of_canonical_bytes(&bytes), bytes)
}

fn blob(text: &str) -> (ContentId, Vec<u8>) {
    (ContentId::of(text.as_bytes()), text.as_bytes().to_vec())
}

fn durable_appends(trace: &PackedStoreTrace) -> u64 {
    trace.snapshot().unwrap().durable_appends
}

#[tokio::test]
async fn staged_objects_and_content_read_as_stored_and_flush_appends_once() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let base_text = "a line that the next version keeps\n".repeat(64);
    let base = content.put(base_text.as_bytes()).unwrap();
    let (object_id, object_bytes) = object();
    let (whole_id, whole) = blob("new content");
    let (changed_id, changed) = blob(&format!("{base_text}and one more line\n"));
    let before = durable_appends(&trace);

    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([
            (VerifiedContent::new(whole.clone()), None),
            (VerifiedContent::new(changed.clone()), Some(base)),
        ])
        .await
        .unwrap();

    assert_eq!(
        stage.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(stage.content().get(whole_id).unwrap(), whole);
    assert_eq!(
        stage.content().logical_length(changed_id).unwrap(),
        changed.len() as u64
    );
    assert_eq!(
        stage
            .content()
            .existing_ids(&[whole_id, changed_id])
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        store.get(object_id).await,
        Err(ObjectStoreError::Missing(_))
    ));
    assert!(!content.contains(whole_id));
    assert!(!content.contains(changed_id));
    assert_eq!(durable_appends(&trace), before);

    stage.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
    assert_eq!(
        store.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(content.get(whole_id).unwrap(), whole);
    assert_eq!(content.get(changed_id).unwrap(), changed);

    // Nothing is left staged to write again.
    stage.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
}

#[tokio::test]
async fn taken_stages_read_beneath_another_stage_and_store_in_one_append() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let (object_id, object_bytes) = object();
    let (blob_id, blob_bytes) = blob("accepted content");
    let before = durable_appends(&trace);

    let first = RepositoryStage::new(&store, 16, 1 << 20);
    first.put_if_absent(object_id, &object_bytes).await.unwrap();
    let second = RepositoryStage::new(&store, 16, 1 << 20);
    second
        .put_content([(VerifiedContent::new(blob_bytes.clone()), None)])
        .await
        .unwrap();
    let mut pending = PendingPublications::default();
    pending.push(first.take().await.unwrap());
    pending.push(second.take().await.unwrap());

    // Taken stages hold nothing more to write.
    first.flush().await.unwrap();
    second.flush().await.unwrap();
    assert_eq!(durable_appends(&trace), before);

    // Another stage sees the pending publications only through the view.
    let third = RepositoryStage::new(&store, 16, 1 << 20);
    assert_eq!(third.exists_many(&[object_id]).await.unwrap(), [false]);
    assert!(third.content().get(blob_id).is_err());
    let view = pending.over(&third);
    assert_eq!(view.exists_many(&[object_id]).await.unwrap(), [true]);
    assert_eq!(
        view.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(view.content().get(blob_id).unwrap(), blob_bytes);

    pending.store(&store).await.unwrap();
    assert_eq!(durable_appends(&trace), before + 1);
    assert_eq!(
        store.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(content.get(blob_id).unwrap(), blob_bytes);
}

/// Content is hashed where it comes in, which makes it a [`VerifiedContent`]; nothing
/// on its way into a store hashes it again. That holds for a flushed stage and for
/// pending publications, into a packed store (durable or deferred, in a segment or,
/// past the loose threshold, as a loose file) and into a loose content store.
#[tokio::test]
async fn verified_content_is_stored_without_hashing_it_again() {
    let dir = tempfile::tempdir().unwrap();
    let durable = PackedObjectStore::open(dir.path().join("durable")).unwrap();
    let deferred = PackedObjectStore::open_deferred(dir.path().join("deferred")).unwrap();
    let separate = SeparateRepositoryStore::new(
        LooseObjectStore::new(dir.path().join("objects")),
        ContentStore::new(dir.path().join("content")),
    );
    let stores: [&dyn RepositoryStore; 3] = [&durable, &deferred, &separate];
    for store in stores {
        let blobs = |seed: u8| {
            [
                VerifiedContent::new(vec![seed; 1024]),
                VerifiedContent::new(vec![seed; 256 * 1024]),
            ]
        };
        let (flushed, pending) = (blobs(1), blobs(2));
        let before = wheat_core::content_bytes_hashed();

        let stage = RepositoryStage::new(store, 16, 1 << 20);
        stage
            .put_content(flushed.iter().map(|content| (content.clone(), None)))
            .await
            .unwrap();
        stage.flush().await.unwrap();
        let taken = RepositoryStage::new(store, 16, 1 << 20);
        taken
            .put_content(pending.iter().map(|content| (content.clone(), None)))
            .await
            .unwrap();
        let mut publications = PendingPublications::default();
        publications.push(taken.take().await.unwrap());
        publications.store(store).await.unwrap();

        assert_eq!(wheat_core::content_bytes_hashed(), before);
        for content in flushed.iter().chain(&pending) {
            assert!(store.content_store().contains(content.id()));
        }
    }
}

/// Staged and batched content is held verified, so reading it back through the stage or
/// the pending publications hashes nothing.
#[tokio::test]
async fn staged_content_reads_back_without_hashing() {
    let dir = tempfile::tempdir().unwrap();
    let store = PackedObjectStore::open(dir.path()).unwrap();
    let staged = VerifiedContent::new(vec![41; 4096]);
    let pending = VerifiedContent::new(vec![42; 4096]);
    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_content([(staged.clone(), None)]).await.unwrap();
    let taken = RepositoryStage::new(&store, 16, 1 << 20);
    taken.put_content([(pending.clone(), None)]).await.unwrap();
    let mut publications = PendingPublications::default();
    publications.push(taken.take().await.unwrap());
    let view = publications.over(&stage);

    let before = wheat_core::content_bytes_hashed();
    assert_eq!(stage.content().get(staged.id()).unwrap(), staged.bytes());
    assert_eq!(view.content().get(staged.id()).unwrap(), staged.bytes());
    assert_eq!(view.content().get(pending.id()).unwrap(), pending.bytes());
    assert_eq!(wheat_core::content_bytes_hashed(), before);
}

#[tokio::test]
async fn content_reaching_the_byte_limit_is_stored_with_the_staged_objects() {
    let dir = tempfile::tempdir().unwrap();
    let (store, trace) = PackedObjectStore::open_traced(dir.path()).unwrap();
    let content = store.content_store();
    let (object_id, object_bytes) = object();
    let (small_id, small) = blob("small");
    let (large_id, large) = blob("larger than the stage holds");
    let before = durable_appends(&trace);

    let stage = RepositoryStage::new(&store, 16, 16);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([(VerifiedContent::new(small), None)])
        .await
        .unwrap();
    assert!(!content.contains(small_id));
    stage
        .put_content([(VerifiedContent::new(large), None)])
        .await
        .unwrap();

    assert_eq!(durable_appends(&trace), before + 1);
    assert!(content.contains(small_id));
    assert!(content.contains(large_id));
    assert!(store.get(object_id).await.is_ok());
}

struct FailingObjects {
    inner: LooseObjectStore,
    fail: AtomicBool,
}

#[async_trait]
impl ImmutableObjectStore for FailingObjects {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ObjectStoreError::Backend("injected".into()));
        }
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[tokio::test]
async fn a_failed_flush_keeps_everything_staged_for_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let store = SeparateRepositoryStore::new(
        FailingObjects {
            inner: LooseObjectStore::new(dir.path().join("objects")),
            fail: AtomicBool::new(true),
        },
        ContentStore::new(dir.path().join("content")),
    );
    let (object_id, object_bytes) = object();
    let (content_id, bytes) = blob("content");
    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([(VerifiedContent::new(bytes.clone()), None)])
        .await
        .unwrap();

    assert!(stage.flush().await.is_err());
    assert!(stage.get(object_id).await.is_ok());
    assert_eq!(stage.content().get(content_id).unwrap(), bytes);

    store.objects().fail.store(false, Ordering::SeqCst);
    stage.flush().await.unwrap();
    assert!(store.get(object_id).await.is_ok());
    assert_eq!(store.content_store().get(content_id).unwrap(), bytes);
}

/// The bytes of every file under `path`.
fn file_bytes(path: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    entries
        .map(|entry| entry.unwrap())
        .map(|entry| {
            if entry.file_type().unwrap().is_dir() {
                file_bytes(&entry.path())
            } else {
                entry.metadata().unwrap().len()
            }
        })
        .sum()
}

fn ids<T: Copy>(added: &[(T, u64)]) -> Vec<T> {
    added.iter().map(|(id, _)| *id).collect()
}

/// A stage counts every byte it is given, and a packed store says which of them it
/// did not hold, with the bytes it keeps for each.
#[tokio::test]
async fn a_stage_counts_what_it_received_and_what_the_store_added() {
    const LOOSE_THRESHOLD: usize = 1024;
    let dir = tempfile::tempdir().unwrap();
    let store = PackedObjectStore::open_with_loose_threshold(dir.path(), LOOSE_THRESHOLD).unwrap();
    let (held_id, held) = blob("content the store holds already");
    store.content_store().put(&held).unwrap();
    let (object_id, object_bytes) = object();
    let (new_id, new) = blob("new content");
    let large = vec![b'x'; 2 * LOOSE_THRESHOLD];
    let large_id = ContentId::of(&large);
    let loose_before = file_bytes(&dir.path().join("loose-content"));

    let stage = RepositoryStage::new(&store, 16, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    stage
        .put_content([
            (VerifiedContent::new(held.clone()), None),
            (VerifiedContent::new(new.clone()), None),
            (VerifiedContent::new(large.clone()), None),
        ])
        .await
        .unwrap();
    assert_eq!(
        stage.received(),
        ReceivedBytes {
            objects: 2 * object_bytes.len() as u64,
            content: (held.len() + new.len() + large.len()) as u64,
        }
    );
    assert_eq!(stage.stored().unwrap(), StoredWrite::default());

    stage.flush().await.unwrap();
    let stored = stage.stored().unwrap();
    assert_eq!(ids(&stored.objects), [object_id]);
    assert!(stored.object_bytes() > 0);
    let mut content = ids(&stored.content);
    content.sort();
    let mut expected = vec![new_id, large_id];
    expected.sort();
    assert_eq!(content, expected, "{held_id} was held already");
    // The large blob is a loose file: what it keeps is that file.
    let large_bytes = stored
        .content
        .iter()
        .find(|(id, _)| *id == large_id)
        .unwrap()
        .1;
    assert_eq!(
        large_bytes,
        file_bytes(&dir.path().join("loose-content")) - loose_before
    );

    // Writing it all again adds nothing.
    let again = RepositoryStage::new(&store, 16, 1 << 20);
    again.put_if_absent(object_id, &object_bytes).await.unwrap();
    again
        .put_content([(VerifiedContent::new(large), None)])
        .await
        .unwrap();
    again.flush().await.unwrap();
    assert_eq!(again.stored().unwrap(), StoredWrite::default());
}

/// A stage that fills up stores what it holds through the same write, so the count
/// includes it.
#[tokio::test]
async fn a_full_stage_counts_what_it_stored_early() {
    let dir = tempfile::tempdir().unwrap();
    let store = PackedObjectStore::open(dir.path()).unwrap();
    let (object_id, object_bytes) = object();

    let stage = RepositoryStage::new(&store, 1, 1 << 20);
    stage.put_if_absent(object_id, &object_bytes).await.unwrap();
    assert!(store.get(object_id).await.is_ok());
    assert_eq!(ids(&stage.stored().unwrap().objects), [object_id]);
}

/// Publications stored together each get what they added, an item two of them
/// held going to the first.
#[tokio::test]
async fn pending_publications_each_get_what_they_added() {
    let dir = tempfile::tempdir().unwrap();
    let store = PackedObjectStore::open(dir.path()).unwrap();
    let (object_id, object_bytes) = object();
    let (blob_id, blob_bytes) = blob("second's content");

    let first = RepositoryStage::new(&store, 16, 1 << 20);
    first.put_if_absent(object_id, &object_bytes).await.unwrap();
    let second = RepositoryStage::new(&store, 16, 1 << 20);
    second
        .put_if_absent(object_id, &object_bytes)
        .await
        .unwrap();
    second
        .put_content([(VerifiedContent::new(blob_bytes), None)])
        .await
        .unwrap();
    let mut pending = PendingPublications::default();
    pending.push(first.take().await.unwrap());
    pending.push(second.take().await.unwrap());

    let added = pending.store(&store).await.unwrap();
    assert_eq!(added.len(), 2);
    assert_eq!(ids(&added[0].objects), [object_id]);
    assert!(added[0].content.is_empty());
    assert!(added[1].objects.is_empty());
    assert_eq!(ids(&added[1].content), [blob_id]);
}

/// Stores kept apart do not say what they added, so each item is looked up first.
#[tokio::test]
async fn a_separate_store_reports_only_what_it_lacked() {
    let dir = tempfile::tempdir().unwrap();
    let store = SeparateRepositoryStore::new(
        LooseObjectStore::new(dir.path().join("objects")),
        ContentStore::new(dir.path().join("content")),
    );
    let (object_id, object_bytes) = object();
    let (blob_id, blob_bytes) = blob("separate content");
    let objects = [(object_id, object_bytes.clone())];
    let content = [ContentItem::new(
        VerifiedContent::new(blob_bytes.as_slice()),
        None,
    )];

    let first = store.put_publication(&objects, &content).await.unwrap();
    assert_eq!(first.objects, [(object_id, object_bytes.len() as u64)]);
    assert_eq!(first.content, [(blob_id, blob_bytes.len() as u64)]);
    let second = store.put_publication(&objects, &content).await.unwrap();
    assert_eq!(second, StoredWrite::default());
}

/// A network's objects go to its pool and its content to the content store every
/// network shares, each in one append, and content another network stored is not
/// stored again.
#[tokio::test]
async fn a_network_store_keeps_objects_in_its_pool_and_content_once_for_every_network() {
    let dir = tempfile::tempdir().unwrap();
    let (content_store, content_trace) =
        PackedObjectStore::open_traced(dir.path().join("content")).unwrap();
    let (first_pool, first_trace) =
        PackedObjectStore::open_traced(dir.path().join("first")).unwrap();
    let (second_pool, second_trace) =
        PackedObjectStore::open_traced(dir.path().join("second")).unwrap();
    let first = NetworkStore::new(first_pool, content_store.clone());
    let second = NetworkStore::new(second_pool, content_store.clone());
    let (object_id, object_bytes) = object();
    let (blob_id, blob_bytes) = blob("content shared by two networks");
    let objects = [(object_id, object_bytes.clone())];
    let content = [ContentItem::new(
        VerifiedContent::new(blob_bytes.as_slice()),
        None,
    )];

    let added = first.put_publication(&objects, &content).await.unwrap();

    assert_eq!(ids(&added.objects), [object_id]);
    assert_eq!(ids(&added.content), [blob_id]);
    assert_eq!(durable_appends(&first_trace), 1);
    assert_eq!(durable_appends(&content_trace), 1);
    assert_eq!(
        first.get(object_id).await.unwrap().canonical_bytes,
        object_bytes
    );
    assert_eq!(first.content_store().get(blob_id).unwrap(), blob_bytes);
    assert!(!first.pool().content_store().contains(blob_id));
    assert!(matches!(
        content_store.get(object_id).await,
        Err(ObjectStoreError::Missing(_))
    ));

    let added = second.put_publication(&objects, &content).await.unwrap();

    assert_eq!(ids(&added.objects), [object_id], "pools are apart");
    assert!(added.content.is_empty(), "content is kept once");
    assert_eq!(durable_appends(&second_trace), 1);
    assert_eq!(durable_appends(&content_trace), 1);
    assert_eq!(second.content_store().get(blob_id).unwrap(), blob_bytes);

    // A write of objects alone or content alone touches only its store.
    let (other_id, other_bytes) = blob("content alone");
    let added = second
        .put_publication(
            &[],
            &[ContentItem::new(
                VerifiedContent::new(other_bytes.as_slice()),
                None,
            )],
        )
        .await
        .unwrap();
    assert_eq!(ids(&added.content), [other_id]);
    assert_eq!(durable_appends(&second_trace), 1);
    assert_eq!(durable_appends(&content_trace), 2);
}
