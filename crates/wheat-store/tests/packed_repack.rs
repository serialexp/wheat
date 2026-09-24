use std::collections::BTreeMap;
use std::fs;
use std::time::Instant;

use wheat_core::{Repository, Tree};
use wheat_store::{
    LocalPublicationStore, PackedObjectStore, PublicationStore, RepackError, RepackOptions,
    RepositoryPublisher,
};

#[tokio::test]
async fn whole_repository_repack_preserves_content_reopens_and_then_is_true_noop() {
    let directory = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(directory.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let retained = content.put(b"retained content").unwrap();
    let unreferenced_but_physical = content.put(b"unreferenced physical content").unwrap();
    let repository = Repository::new(Tree::new());
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();

    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    assert!(plan.estimated_replacement_content_bytes > 0);
    let report = objects.execute_repack(&heads, plan).unwrap();
    assert!(!report.no_op);
    assert_eq!(report.envelope_before, report.envelope_after);
    assert_eq!(content.get(retained).unwrap(), b"retained content");
    assert_eq!(
        content.get(unreferenced_but_physical).unwrap(),
        b"unreferenced physical content"
    );
    // Exercise the cached reader repeatedly on the same handle, not just after reopen.
    for _ in 0..32 {
        assert_eq!(content.get(retained).unwrap(), b"retained content");
        assert_eq!(
            content.get(unreferenced_but_physical).unwrap(),
            b"unreferenced physical content"
        );
    }
    drop(content);
    drop(objects);

    let reopened = PackedObjectStore::open(directory.path().join("storage")).unwrap();
    let reopened_content = reopened.content_store();
    assert_eq!(reopened_content.get(retained).unwrap(), b"retained content");
    let plan = reopened
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    let second = reopened.execute_repack(&heads, plan).unwrap();
    assert!(second.no_op);
    assert_eq!(
        second.physical_generation_before,
        second.physical_generation_after
    );
    // A no-op writes and retires nothing, including content packs.
    assert_eq!((second.bytes_written, second.bytes_retired), (0, 0));
}

#[tokio::test]
async fn repack_retires_manifest_covered_loose_content_but_preserves_later_fallbacks() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let covered_bytes = vec![b'a'; 256 * 1024];
    let covered = content.put(&covered_bytes).unwrap();
    let covered_path = content.path(covered);
    assert!(covered_path.is_file());
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();

    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(&heads, plan).unwrap();
    assert!(!covered_path.exists());
    assert_eq!(content.get(covered).unwrap(), covered_bytes);

    // This write races only logically with cleanup: it is not named by the
    // committed manifest, so it remains a loose fallback until a later repack.
    let later_bytes = vec![b'b'; 256 * 1024];
    let later = content.put(&later_bytes).unwrap();
    let later_path = content.path(later);
    assert!(later_path.is_file());
    assert_eq!(content.get(later).unwrap(), later_bytes);
    drop(content);
    drop(objects);

    let reopened = PackedObjectStore::open(&storage).unwrap();
    let reopened_content = reopened.content_store();
    assert_eq!(
        reopened_content.get(covered).unwrap(),
        vec![b'a'; 256 * 1024]
    );
    assert_eq!(reopened_content.get(later).unwrap(), vec![b'b'; 256 * 1024]);
    assert!(later_path.is_file());
}

#[tokio::test]
async fn same_length_corrupt_generation_never_deletes_loose_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let id = content.put(&vec![b'x'; 256 * 1024]).unwrap();
    let loose_path = content.path(id);
    let loose_bytes = fs::read(&loose_path).unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(&heads, plan).unwrap();
    assert!(!loose_path.exists());
    fs::create_dir_all(loose_path.parent().unwrap()).unwrap();
    fs::write(&loose_path, loose_bytes).unwrap();
    let pack = storage.join("content-generations/0/shard-000000.pack");
    let mut bytes = fs::read(&pack).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(pack, bytes).unwrap(); // unchanged length, changed authenticated bytes
    drop(content);
    drop(objects);

    assert!(PackedObjectStore::open(&storage).is_err());
    assert!(loose_path.is_file());
}

#[tokio::test]
async fn open_retries_interrupted_post_head_content_retirement() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let bytes = vec![b'c'; 256 * 1024];
    let id = content.put(&bytes).unwrap();
    let loose_path = content.path(id);
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(&heads, plan).unwrap();
    assert!(!loose_path.exists());
    // Recreate the redundant pre-retirement fallback after a committed head,
    // modelling an interrupted cleanup without modifying authority.
    fs::create_dir_all(loose_path.parent().unwrap()).unwrap();
    fs::write(&loose_path, b"redundant interrupted cleanup debris").unwrap();
    drop(content);
    drop(objects);

    let reopened = PackedObjectStore::open(&storage).unwrap();
    let reopened_content = reopened.content_store();
    assert_eq!(reopened_content.get(id).unwrap(), bytes);
    assert!(!loose_path.exists());
}

#[tokio::test]
async fn generation_only_content_survives_a_second_repack() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let id = content
        .put(b"only in the selected generation after segment retirement")
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(&heads, plan).unwrap();
    // The original small content lived in a segment, not a loose file.
    // The first repack retires it, leaving the selected generation as sole home.
    assert_eq!(
        content.get(id).unwrap(),
        b"only in the selected generation after segment retirement"
    );
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    let report = objects.execute_repack(&heads, plan).unwrap();
    assert!(report.no_op);
    assert_eq!(
        content.get(id).unwrap(),
        b"only in the selected generation after segment retirement"
    );
}

fn file_inventory(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
    ) {
        let mut entries = fs::read_dir(path)
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), files);
            } else {
                files.insert(
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[tokio::test]
async fn planning_is_read_only_for_all_physical_authority_and_files() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    content
        .put(b"active content that planning must not seal")
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let before = file_inventory(&storage);

    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();

    assert_eq!(file_inventory(&storage), before);
    assert!(plan.estimated_replacement_content_bytes > 0);
}

#[tokio::test]
async fn recovery_preserves_unknown_and_malformed_content_generation_debris() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    content.put(b"retained").unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    objects.execute_repack(&heads, plan).unwrap();
    drop(content);
    drop(objects);

    let generations = storage.join("content-generations");
    let unknown = generations.join("operator-notes");
    let malformed = generations.join("999");
    fs::create_dir_all(&unknown).unwrap();
    fs::create_dir_all(&malformed).unwrap();
    fs::write(unknown.join("keep"), b"unknown").unwrap();
    fs::write(malformed.join("content.pack"), b"not authenticated").unwrap();

    let reopened = PackedObjectStore::open(&storage).unwrap();
    assert_eq!(fs::read(unknown.join("keep")).unwrap(), b"unknown");
    assert_eq!(
        fs::read(malformed.join("content.pack")).unwrap(),
        b"not authenticated"
    );
    drop(reopened);
}

#[test]
#[ignore = "representative loose-content cleanup benchmark; run explicitly with --ignored --nocapture"]
fn retire_repacked_content_scales_to_90k_files() {
    const COUNT: usize = 90_812;
    let directory = tempfile::tempdir().unwrap();
    let loose = directory.path().join("loose-content");
    let started = Instant::now();
    for index in 0..COUNT {
        let id = wheat_core::ContentId::of(&index.to_le_bytes());
        let encoded = id.to_string();
        let path = loose.join(&encoded[..2]).join(&encoded[2..]);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"x").unwrap();
    }
    let write_elapsed = started.elapsed();
    let removed_started = Instant::now();
    for prefix in fs::read_dir(&loose).unwrap() {
        let prefix = prefix.unwrap();
        for file in fs::read_dir(prefix.path()).unwrap() {
            fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::File::open(prefix.path())
            .unwrap()
            .sync_all()
            .unwrap();
    }
    println!(
        "packed loose cleanup benchmark: files={COUNT} setup_ms={} cleanup_ms={} synced_shards<=256",
        write_elapsed.as_millis(),
        removed_started.elapsed().as_millis()
    );
}

#[tokio::test]
async fn execute_repack_rejects_a_stale_repository_head() {
    let directory = tempfile::tempdir().unwrap();
    let objects = PackedObjectStore::open(directory.path().join("storage")).unwrap();
    let content = objects.content_store();
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    let repository = Repository::new(Tree::new());
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    heads
        .compare_and_advance(head, head.next(head.envelope).unwrap())
        .unwrap();

    assert!(matches!(
        objects.execute_repack(&heads, plan),
        Err(RepackError::StaleAuthority)
    ));
}
