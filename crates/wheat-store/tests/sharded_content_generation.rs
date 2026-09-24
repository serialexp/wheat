//! A content generation larger than one shard: repack splits it into shard
//! manifests and packs, deltas may use a base in another shard, reads and
//! validation survive reopen, a second repack is a no-op, and damage to any
//! one shard file is caught at open.

use std::fs;
use std::path::Path;

use wheat_core::*;
use wheat_store::*;

/// Enough unreferenced content to fill one shard and half of a second, so the
/// tracked versions' (uniformly spread) IDs land in both.
const FILLER: usize = CONTENT_GENERATION_SHARD_ENTRIES + CONTENT_GENERATION_SHARD_ENTRIES / 2;
/// Versions of one tracked file, so the generation has delta chains.
const VERSIONS: usize = 12;

fn version(n: usize) -> Vec<u8> {
    let mut bytes = (0..8 * 1024)
        .map(|offset| ((offset * 131 + offset / 7) % 251) as u8)
        .collect::<Vec<_>>();
    bytes[100..108].copy_from_slice(&(n as u64).to_be_bytes());
    bytes
}

fn filler(n: usize) -> Vec<u8> {
    format!("filler content {n}").into_bytes()
}

fn file(bytes: &[u8]) -> FileValue {
    FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    }
}

fn replace(expected: &[u8], value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        "tracked",
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

/// Writes the filler content with deferred durability, which keeps ~17k
/// small writes fast; `close` makes it durable.
fn write_filler(storage: &Path) {
    let writer = PackedObjectStore::open_deferred(storage).unwrap();
    let content = writer.content_store();
    for n in 0..FILLER {
        content.put(&filler(n)).unwrap();
    }
    drop(content);
    writer.close().unwrap();
}

/// Publishes a linear history of `tracked`.
async fn publish(objects: &PackedObjectStore, heads: &LocalPublicationStore) -> RepositoryClosure {
    let content = objects.content_store();
    content.put(&version(0)).unwrap();
    let mut repo = Repository::new([("tracked".to_owned(), file(&version(0)))].into());
    let basis = repo.base_view().id;
    let mut effective = basis;
    let mut previous = None;
    for n in 1..VERSIONS {
        content.put(&version(n)).unwrap();
        // Each change depends on the last, so it edits the previous version.
        let (change, outcome) = repo
            .create_change(
                basis,
                effective,
                previous,
                replace(&version(n - 1), &version(n)),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
        previous = Some(change);
    }
    let publisher = RepositoryPublisher::new(objects, &content, heads);
    let closure = publisher.prepare(&repo).await.unwrap();
    publisher.initialize_root(closure.state.id).await.unwrap();
    closure
}

fn current_generation(storage: &Path) -> ContentGenerationManifest {
    let generations = storage.join("content-generations");
    let index = fs::read_dir(&generations)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "manifest")
        })
        .expect("one current index");
    let stem = index.file_stem().unwrap().to_str().unwrap().to_owned();
    let (generation, digest) = stem.split_once('-').unwrap();
    read_content_generation(
        &generations,
        PhysicalContentGeneration {
            generation: generation.parse().unwrap(),
            manifest: hex::decode(digest).unwrap().try_into().unwrap(),
        },
    )
    .unwrap()
}

async fn assert_reads_back(objects: &PackedObjectStore, closure: &RepositoryClosure) {
    let content = objects.content_store();
    for n in (0..FILLER).step_by(97).chain([FILLER - 1]) {
        assert_eq!(content.get(ContentId::of(&filler(n))).unwrap(), filler(n));
    }
    for n in 0..VERSIONS {
        assert_eq!(content.get(ContentId::of(&version(n))).unwrap(), version(n));
    }
    RepositoryReader::new(objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
}

/// Flip one byte of `path`, check `open` rejects the store, then restore it.
fn assert_open_rejects_damaged(storage: &Path, path: &Path) {
    let original = fs::read(path).unwrap();
    let mut damaged = original.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    fs::write(path, &damaged).unwrap();
    assert!(
        PackedObjectStore::open(storage).is_err(),
        "{} damage must be caught",
        path.display()
    );
    fs::write(path, original).unwrap();
}

#[tokio::test]
async fn generation_larger_than_one_shard_round_trips_and_detects_shard_damage() {
    let directory = tempfile::tempdir().unwrap();
    let storage = directory.path().join("storage");
    let heads = LocalPublicationStore::new(directory.path().join("repository.head"));
    write_filler(&storage);
    let objects = PackedObjectStore::open(&storage).unwrap();
    let closure = publish(&objects, &heads).await;

    let plan = objects
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    let report = objects.execute_repack(&heads, plan).unwrap();
    assert!(!report.no_op);
    assert_eq!(report.content_record_count, (FILLER + VERSIONS) as u64);
    assert!(
        report.delta_content_after >= (VERSIONS - 1) as u64,
        "{report:?}"
    );
    assert_reads_back(&objects, &closure).await;

    let manifest = current_generation(&storage);
    assert_eq!(manifest.shard_count(), 2);
    let generation_dir = storage
        .join("content-generations")
        .join(manifest.generation.to_string());
    let mut files = fs::read_dir(&generation_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        [
            "shard-000000.manifest",
            "shard-000000.pack",
            "shard-000001.manifest",
            "shard-000001.pack"
        ]
    );
    // Each pack starts at offset zero and holds exactly its shard's entries.
    for shard in 0..manifest.shard_count() {
        let entries = manifest.shard_entries(shard);
        assert!(entries.iter().any(|entry| entry.pack_offset == 0));
        assert_eq!(
            fs::metadata(generation_dir.join(format!("shard-{shard:06}.pack")))
                .unwrap()
                .len(),
            entries
                .iter()
                .map(|entry| entry.physical_length)
                .sum::<u64>()
        );
    }
    // The version chain's IDs are spread over both shards, so at least one
    // delta reads its base from the other shard.
    let shard_of = |id: ContentId| {
        ContentGenerationManifest::shard_of(
            manifest
                .entries
                .binary_search_by_key(&id, |entry| entry.id)
                .unwrap(),
        )
    };
    assert!(
        manifest.entries.iter().any(|entry| entry
            .delta_base
            .is_some_and(|base| shard_of(base) != shard_of(entry.id))),
        "the fixture must exercise a cross-shard delta base"
    );
    drop(objects);

    let reopened = PackedObjectStore::open(&storage).unwrap();
    assert_reads_back(&reopened, &closure).await;
    let plan = reopened
        .plan_repack(&heads, RepackOptions::default())
        .await
        .unwrap();
    let second = reopened.execute_repack(&heads, plan).unwrap();
    assert!(second.no_op, "{second:?}");
    assert_eq!(current_generation(&storage), manifest);
    drop(reopened);

    for file in files {
        assert_open_rejects_damaged(&storage, &generation_dir.join(file));
    }
    let index = content_generation_index_path(
        &storage.join("content-generations"),
        manifest.generation,
        &manifest
            .index()
            .unwrap()
            .encoded()
            .map(|bytes| *blake3::hash(&bytes).as_bytes())
            .unwrap(),
    );
    assert!(index.is_file());
    assert_open_rejects_damaged(&storage, &index);
    let missing = generation_dir.join("shard-000001.pack");
    let pack = fs::read(&missing).unwrap();
    fs::remove_file(&missing).unwrap();
    assert!(PackedObjectStore::open(&storage).is_err());
    fs::write(&missing, pack).unwrap();
    let restored = PackedObjectStore::open(&storage).unwrap();
    assert_reads_back(&restored, &closure).await;
}
