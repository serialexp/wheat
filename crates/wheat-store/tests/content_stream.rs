//! Large blobs stored as content-defined chunks through `ContentStore::writer` and
//! read back through `ContentStore::reader`.

use std::io::{Read, Write};
use std::path::Path;

use wheat_core::{CHUNKED_CONTENT_THRESHOLD, ContentId, MAX_CONTENT_CHUNK_BYTES, Repository, Tree};
use wheat_store::{
    ContentStore, ContentStoreError, LocalPublicationStore, PackedObjectStore, RepackOptions,
    RepositoryPublisher,
};

const MIB: usize = 1 << 20;
const THRESHOLD: usize = CHUNKED_CONTENT_THRESHOLD as usize;
const MAX_CHUNK: usize = MAX_CONTENT_CHUNK_BYTES as usize;

/// `len` bytes that don't compress, from `seed`.
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

/// Writes `bytes` in pieces of `piece` bytes, returning the ID and the most blob
/// bytes the writer held at once.
fn write(store: &ContentStore, bytes: &[u8], piece: usize) -> (ContentId, usize) {
    let mut writer = store.writer();
    let mut held = 0;
    for part in bytes.chunks(piece) {
        writer.write_all(part).unwrap();
        held = held.max(writer.held_bytes());
    }
    (writer.finish(Some(ContentId::of(bytes))).unwrap(), held)
}

/// Reads `id` back through a reader with a small buffer, returning the bytes and the
/// most blob bytes the reader held at once.
fn read(store: &ContentStore, id: ContentId) -> (Vec<u8>, usize) {
    let mut reader = store.reader(id).unwrap();
    let mut out = Vec::new();
    let mut buffer = vec![0; 64 * 1024];
    let mut held = reader.held_bytes();
    loop {
        let read = reader.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..read]);
        held = held.max(reader.held_bytes());
    }
    assert_eq!(out.len() as u64, reader.len());
    (out, held)
}

/// Every file under `root`, by path.
fn files(root: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
    let mut found = std::collections::BTreeSet::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.insert(path);
            }
        }
    }
    found
}

#[test]
fn a_large_blob_is_written_and_read_in_chunks_holding_at_most_one() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(1, 40 * MIB + 12_345);

    // Fed in pieces that do not line up with the threshold or the chunks.
    let (id, written_held) = write(&store, &bytes, 3 * MIB + 7);
    assert_eq!(id, ContentId::of(&bytes));
    assert!(
        written_held <= THRESHOLD,
        "the writer held {written_held} bytes"
    );
    // The blob's own file is its manifest; the chunks are content of their own.
    assert!(std::fs::metadata(store.path(id)).unwrap().len() < 4096);
    assert_eq!(store.logical_length(id).unwrap(), bytes.len() as u64);

    let (read_back, read_held) = read(&store, id);
    assert!(read_back == bytes);
    assert!(read_held <= MAX_CHUNK, "the reader held {read_held} bytes");
    assert!(store.get(id).unwrap() == bytes);
    assert!(matches!(
        store.get_bounded(id, bytes.len() - 1),
        Err(ContentStoreError::TooLarge { minimum, .. }) if minimum == bytes.len()
    ));
}

#[test]
fn once_past_the_threshold_the_writer_holds_at_most_one_chunk() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(2, 48 * MIB);
    let mut writer = store.writer();
    let mut held_after = 0;
    for (index, part) in bytes.chunks(MIB).enumerate() {
        writer.write_all(part).unwrap();
        if index > 16 {
            held_after = held_after.max(writer.held_bytes());
        }
    }
    assert!(
        held_after <= MAX_CHUNK,
        "the writer held {held_after} bytes"
    );
    assert_eq!(writer.finish(None).unwrap(), ContentId::of(&bytes));
}

#[test]
fn a_blob_up_to_the_threshold_is_stored_whole() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(3, THRESHOLD);
    let (id, held) = write(&store, &bytes, MIB);
    assert!(held <= THRESHOLD);
    // One envelope with the bytes in it, and nothing else.
    assert_eq!(files(directory.path()).len(), 1);
    assert!(std::fs::metadata(store.path(id)).unwrap().len() > bytes.len() as u64);
    assert!(read(&store, id).0 == bytes);
}

#[test]
fn a_small_edit_to_a_large_blob_stores_only_the_chunks_it_touched() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(4, 64 * MIB);
    let (first, _) = write(&store, &bytes, MIB);
    let before = files(directory.path());

    let mut edited = bytes.clone();
    edited[40 * MIB] ^= 1;
    let (second, _) = write(&store, &edited, MIB);
    assert_ne!(first, second);
    let added = files(directory.path())
        .difference(&before)
        .map(|path| std::fs::metadata(path).unwrap().len())
        .collect::<Vec<_>>();
    // The new manifest and the one or two chunks around the edit.
    assert!((2..=3).contains(&added.len()), "{added:?}");
    assert!(added.iter().sum::<u64>() <= 2 * MAX_CHUNK as u64 + 64 * 1024);
    assert!(read(&store, second).0 == edited);
    assert!(read(&store, first).0 == bytes);
}

#[test]
fn put_stores_a_large_blob_as_the_writer_does() {
    let written = tempfile::tempdir().unwrap();
    let put = tempfile::tempdir().unwrap();
    let bytes = noise(5, 20 * MIB);
    let (id, _) = write(&ContentStore::new(written.path()), &bytes, MIB);
    assert_eq!(ContentStore::new(put.path()).put(&bytes).unwrap(), id);
    let contents = |root: &Path| {
        files(root)
            .into_iter()
            .map(|path| {
                (
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert!(contents(written.path()) == contents(put.path()));
}

#[test]
fn a_writer_refuses_bytes_that_are_not_the_expected_blob() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(6, 20 * MIB);
    let mut writer = store.writer();
    writer.write_all(&bytes).unwrap();
    let wrong = ContentId::of(b"something else");
    assert!(matches!(
        writer.finish(Some(wrong)),
        Err(ContentStoreError::Corrupt { expected, .. }) if expected == wrong
    ));
    assert!(!store.contains(ContentId::of(&bytes)));
    assert!(!store.contains(wrong));
}

#[test]
fn a_reader_refuses_a_chunk_that_is_not_what_the_manifest_names() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let bytes = noise(7, 20 * MIB);
    let (id, _) = write(&store, &bytes, MIB);
    let manifest_path = store.path(id);
    // Swap one chunk's file for another chunk's: each is a valid envelope, but not
    // of the bytes the manifest names.
    let chunks = files(directory.path())
        .into_iter()
        .filter(|path| *path != manifest_path)
        .collect::<Vec<_>>();
    assert!(chunks.len() >= 2);
    std::fs::copy(&chunks[1], &chunks[0]).unwrap();

    let mut reader = store.reader(id).unwrap();
    let mut sink = Vec::new();
    // Refused as corrupt, or as longer than the chunk it stands in for.
    let error = reader.read_to_end(&mut sink).unwrap_err().to_string();
    assert!(
        error.contains("integrity") || error.contains("read limit"),
        "{error}"
    );
    assert!(store.get(id).is_err());
}

#[test]
fn a_chunked_blob_is_no_delta_base() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path());
    let large = noise(10, 20 * MIB);
    let (base, _) = write(&store, &large, MIB);
    // Close to the base's start, so a delta against it would be small.
    let mut small = large[..MIB].to_vec();
    small[100] ^= 1;
    let small = wheat_core::VerifiedContent::new(small.as_slice());
    store.put_with_base(small, base).unwrap();
    assert_eq!(store.get(small.id()).unwrap(), small.bytes());
    assert!(std::fs::metadata(store.path(small.id())).unwrap().len() > MIB as u64);
}

/// A publication's large blob is stored as loose chunks and a manifest, each
/// accounted for in what the write reports.
#[tokio::test(flavor = "multi_thread")]
async fn a_packed_publication_stores_and_accounts_for_a_large_blobs_chunks() {
    use wheat_store::{ContentItem, RepositoryStore};

    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let bytes = noise(11, 30 * MIB);
    let item = ContentItem::new(wheat_core::VerifiedContent::new(bytes.as_slice()), None);
    let write = objects.put_publication(&[], &[item]).await.unwrap();

    let loose = files(&storage.join("loose-content"));
    assert!(loose.len() >= 8, "{} loose files", loose.len());
    assert_eq!(write.content.len(), loose.len());
    let on_disk = loose
        .iter()
        .map(|path| std::fs::metadata(path).unwrap().len())
        .sum::<u64>();
    assert_eq!(
        write.content.iter().map(|(_, length)| *length).sum::<u64>(),
        on_disk
    );
    let content = objects.content_store();
    assert!(std::fs::metadata(content.path(item.id())).unwrap().len() < 4096);
    assert!(read(&content, item.id()).0 == bytes);

    // Stored already: a second publication writes nothing.
    let again = objects.put_publication(&[], &[item]).await.unwrap();
    assert!(again.content.is_empty());
    assert_eq!(files(&storage.join("loose-content")), loose);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_packed_store_keeps_chunked_blobs_through_a_repack() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let bytes = noise(8, 72 * MIB);
    let (id, _) = write(&content, &bytes, MIB);
    let mut edited = bytes.clone();
    edited[10 * MIB] ^= 1;
    let (edited_id, _) = write(&content, &edited, MIB);
    assert!(read(&content, id).0 == bytes);
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();

    // Over the planner's 64 MiB record bound: repack never reads the blob whole.
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    assert!(!objects.execute_repack(&heads, plan).unwrap().no_op);
    // The manifests moved into the content generation.
    assert!(!content.path(id).exists());
    assert!(!content.path(edited_id).exists());
    drop(content);
    drop(objects);

    let reopened = PackedObjectStore::open(&storage).unwrap();
    let content = reopened.content_store();
    assert_eq!(content.logical_length(id).unwrap(), bytes.len() as u64);
    let (read_back, held) = read(&content, id);
    assert!(read_back == bytes);
    assert!(held <= MAX_CHUNK);
    assert!(read(&content, edited_id).0 == edited);
    let plan = reopened
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    assert!(reopened.execute_repack(&heads, plan).unwrap().no_op);
}
