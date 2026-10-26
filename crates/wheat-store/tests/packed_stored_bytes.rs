//! A packed store keeps what its files take on disk as they come and go, so asking
//! for its size lists no directory.

use std::{fs, path::Path};

use wheat_core::*;
use wheat_store::*;

/// Files in the published tree, each one directory entry.
const WIDTH: usize = 48;
/// Objects and content over this many bytes are written loose.
const LOOSE_THRESHOLD: usize = 256;

fn small_content(index: usize) -> Vec<u8> {
    format!("small {index}").into_bytes()
}

/// A tree whose root directory is too wide for a segment.
fn wide_tree() -> Tree {
    (0..WIDTH)
        .map(|index| {
            (
                format!("file{index:02}"),
                FileValue {
                    content: ContentId::of(&small_content(index)),
                    kind: FileKind::Regular,
                },
            )
        })
        .collect()
}

/// Publishes a repository with loose objects and loose content in it.
async fn publish(objects: &PackedObjectStore, heads: &LocalPublicationStore) {
    let content = objects.content_store();
    for index in 0..WIDTH {
        content.put(&small_content(index)).unwrap();
    }
    content.put(&vec![b'a'; 4 * LOOSE_THRESHOLD]).unwrap();
    RepositoryPublisher::new(objects, &content, heads)
        .initialize(&Repository::new(wide_tree()))
        .await
        .unwrap();
}

/// The bytes of every file under `path`.
fn file_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
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

/// What the store's files take, measured by listing them.
fn listed(storage: &Path) -> StoredBytes {
    let packed = storage.join("packed");
    StoredBytes {
        segments: file_bytes(&packed.join("segments")),
        active_segment: fs::metadata(packed.join("active.segment")).map_or(0, |file| file.len()),
        content_generation: file_bytes(&storage.join("content-generations")),
        loose_objects: file_bytes(&storage.join("loose-objects")),
        loose_content: file_bytes(&storage.join("loose-content")),
    }
}

async fn repack(objects: &PackedObjectStore, heads: &LocalPublicationStore) {
    let plan = objects
        .plan_repack(heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(heads, plan).unwrap();
}

#[tokio::test]
async fn stored_bytes_follow_writes_reopen_and_repack() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let objects = PackedObjectStore::open_with_loose_threshold(&storage, LOOSE_THRESHOLD).unwrap();
    assert_eq!(objects.stored_bytes().unwrap(), listed(&storage));

    publish(&objects, &heads).await;
    let written = objects.stored_bytes().unwrap();
    assert!(written.loose_objects > 0, "{written:?}");
    assert!(written.loose_content > 0, "{written:?}");
    assert_eq!(written, listed(&storage));
    drop(objects);

    let reopened = PackedObjectStore::open_with_loose_threshold(&storage, LOOSE_THRESHOLD).unwrap();
    assert_eq!(reopened.stored_bytes().unwrap(), written);

    repack(&reopened, &heads).await;
    let repacked = reopened.stored_bytes().unwrap();
    assert_eq!(repacked.loose_objects, 0, "{repacked:?}");
    assert_eq!(repacked.loose_content, 0, "{repacked:?}");
    assert!(repacked.content_generation > 0, "{repacked:?}");
    assert_eq!(repacked, listed(&storage));
    assert_eq!(
        repacked.total(),
        repacked.segments
            + repacked.active_segment
            + repacked.content_generation
            + repacked.loose_objects
            + repacked.loose_content
    );
    drop(reopened);

    let reopened = PackedObjectStore::open_with_loose_threshold(&storage, LOOSE_THRESHOLD).unwrap();
    assert_eq!(reopened.stored_bytes().unwrap(), repacked);
}

/// The catalog's index is derived: without it, open lists the loose directories
/// once, and removes what crashed writes left there.
#[tokio::test]
async fn a_lost_index_relists_the_loose_files() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let objects = PackedObjectStore::open_with_loose_threshold(&storage, LOOSE_THRESHOLD).unwrap();
    publish(&objects, &heads).await;
    let written = objects.stored_bytes().unwrap();
    drop(objects);

    for name in [
        "catalog-locator.sqlite3",
        "catalog-locator.sqlite3-wal",
        "catalog-locator.sqlite3-shm",
    ] {
        let path = storage.join("packed").join(name);
        if path.exists() {
            fs::remove_file(path).unwrap();
        }
    }
    let shard = fs::read_dir(storage.join("loose-content"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let leftover = shard.join(format!(".{}.4242.0.tmp", "ab".repeat(32)));
    fs::write(&leftover, b"a crashed write").unwrap();

    let reopened = PackedObjectStore::open_with_loose_threshold(&storage, LOOSE_THRESHOLD).unwrap();
    assert!(!leftover.exists());
    assert_eq!(reopened.stored_bytes().unwrap(), written);
}
