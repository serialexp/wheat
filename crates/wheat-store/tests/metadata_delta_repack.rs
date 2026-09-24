//! Repack stores older metadata as in-segment deltas against the newer
//! version, absorbs loose metadata, and stays byte-exact across reopen.

use std::path::Path;

use wheat_core::*;
use wheat_store::*;

/// Files per directory; large enough that one changed entry is a small delta.
const WIDTH: usize = 48;

fn file(bytes: &[u8]) -> FileValue {
    FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    }
}

fn initial_bytes(index: usize) -> Vec<u8> {
    format!("initial {index}").into_bytes()
}

fn base_tree() -> Tree {
    (0..WIDTH)
        .flat_map(|index| {
            [
                (format!("top{index:02}"), file(&initial_bytes(index))),
                (
                    format!("dir/nested/file{index:02}"),
                    file(&initial_bytes(index)),
                ),
            ]
        })
        .collect()
}

fn replace(path: &str, expected: &[u8], value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: Some(file(expected)),
            replacement: Some(file(value)),
        },
    )
    .unwrap();
    let id = record.id();
    RevisionEdits {
        edits: [(id, record)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

/// Builds a linear history of `views` edits to the same nested directory and
/// publishes it through `objects`.
async fn publish_history(
    objects: &PackedObjectStore,
    heads: &LocalPublicationStore,
    views: usize,
) -> RepositoryClosure {
    let content = objects.content_store();
    for index in 0..WIDTH {
        content.put(&initial_bytes(index)).unwrap();
    }
    let mut repo = Repository::new(base_tree());
    let basis = repo.base_view().id;
    let mut effective = basis;
    for step in 0..views {
        let index = step % WIDTH;
        let path = format!("dir/nested/file{index:02}");
        let previous = if step < WIDTH {
            initial_bytes(index)
        } else {
            format!("step {}", step - WIDTH).into_bytes()
        };
        let value = format!("step {step}").into_bytes();
        content.put(&value).unwrap();
        let (_, outcome) = repo
            .create_change(
                basis,
                effective,
                [],
                replace(&path, &previous, &value),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
    }
    let publisher = RepositoryPublisher::new(objects, &content, heads);
    let closure = publisher.prepare(&repo).await.unwrap();
    publisher.initialize_root(closure.state.id).await.unwrap();
    closure
}

async fn assert_closure_reads_back(objects: &PackedObjectStore, closure: &RepositoryClosure) {
    for object in &closure.objects {
        let stored = objects.get(object.id).await.unwrap();
        assert_eq!(stored.canonical_bytes.as_ref(), object.bytes.as_slice());
    }
    let content = objects.content_store();
    RepositoryReader::new(objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
}

async fn repack(objects: &PackedObjectStore, heads: &LocalPublicationStore) -> RepackReport {
    let plan = objects
        .plan_repack(heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(heads, plan).unwrap()
}

fn loose_metadata_files(storage: &Path) -> usize {
    fn count(path: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .map(|entry| entry.unwrap().path())
            .map(|path| if path.is_dir() { count(&path) } else { 1 })
            .sum()
    }
    count(&storage.join("loose-objects"))
}

#[tokio::test]
async fn history_repack_writes_bounded_delta_chains_that_survive_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let objects = PackedObjectStore::open(&storage).unwrap();
    // More sequential views than one chain may hold, so chains restart whole.
    let closure = publish_history(&objects, &heads, 40).await;

    let report = repack(&objects, &heads).await;
    assert!(!report.no_op);
    assert!(report.validated);
    assert!(
        report.metadata_delta_records >= 39,
        "every older directory version can be a delta: {report:?}"
    );
    assert_eq!(
        report.max_metadata_delta_depth, SEGMENT_MAX_DELTA_DEPTH,
        "a 40-view chain must be cut at the depth limit"
    );
    assert!(report.physical_bytes_after < report.physical_bytes_before);
    assert_closure_reads_back(&objects, &closure).await;
    drop(objects);

    let reopened = PackedObjectStore::open(&storage).unwrap();
    assert_closure_reads_back(&reopened, &closure).await;
    let second = repack(&reopened, &heads).await;
    assert!(second.no_op, "{second:?}");
    assert_eq!(second.metadata_delta_records, report.metadata_delta_records);
    assert_eq!(
        second.physical_generation_before,
        second.physical_generation_after
    );
}

#[tokio::test]
async fn repack_absorbs_loose_metadata_and_retires_its_files() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    // Everything over 256 bytes (every wide directory) is written loose.
    let objects = PackedObjectStore::open_with_loose_threshold(&storage, 256).unwrap();
    let closure = publish_history(&objects, &heads, 8).await;
    let loose_before = loose_metadata_files(&storage);
    assert!(loose_before >= 8, "{loose_before}");

    let report = repack(&objects, &heads).await;
    assert_eq!(report.loose_metadata_absorbed, loose_before as u64);
    assert!(report.metadata_delta_records > 0, "{report:?}");
    assert_eq!(loose_metadata_files(&storage), 0);
    assert_closure_reads_back(&objects, &closure).await;
    drop(objects);

    let reopened = PackedObjectStore::open_with_loose_threshold(&storage, 256).unwrap();
    assert_closure_reads_back(&reopened, &closure).await;
    let second = repack(&reopened, &heads).await;
    assert!(second.no_op, "{second:?}");
    assert_eq!(second.loose_metadata_absorbed, 0);
}
