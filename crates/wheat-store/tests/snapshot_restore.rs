use std::{
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use wheat_core::{
    ApplicationContext, ChangeId, ContentId, EditOperation, EditRecord, EditTransition, FileKind,
    FileValue, Repository, RevisionEdits, Tree,
};
use wheat_store::{
    ContentBatch, ContentStore, ImmutableObjectStore, InventoryEntry, LocalPublicationStore,
    LooseObjectStore, MemorySnapshotSink, NativeSnapshot, ObjectBatch, PublicationStore,
    RepositoryPublisher, RepositoryReader, SnapshotError, SnapshotId, SnapshotLimits,
    SnapshotManifest, SnapshotSink, SnapshotSource, StoredObject, export_snapshot,
    export_snapshot_to, prove_union_liveness, restore_snapshot, restore_snapshot_from,
};

fn stores(root: &std::path::Path) -> (LooseObjectStore, ContentStore, LocalPublicationStore) {
    (
        LooseObjectStore::new(root.join("objects")),
        ContentStore::new(root.join("content")),
        LocalPublicationStore::new(root.join("repository.head")),
    )
}

fn edit(path: &str, value: &[u8]) -> RevisionEdits {
    let record = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
                kind: FileKind::Regular,
            }),
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

struct Fixture {
    _root: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    repository: Repository,
    payloads: Vec<Vec<u8>>,
}

async fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(root.path());
    let payloads = vec![
        b"shared source payload".to_vec(),
        vec![b'x'; 257],
        vec![b'y'; 513],
    ];
    for payload in &payloads {
        content.put(payload).unwrap();
    }
    let mut base = Tree::new();
    base.insert(
        "README.md".into(),
        FileValue {
            content: ContentId::of(&payloads[0]),
            kind: FileKind::Regular,
        },
    );
    let mut repository = Repository::new(base);
    let basis = repository.base_view().id;
    let first = repository
        .create_change_with_id(
            basis,
            basis,
            ChangeId::from_str("01890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("src/main.rs", &payloads[1]),
            ApplicationContext::new(&content),
        )
        .unwrap();
    repository
        .create_change_with_id(
            first.resulting_view,
            first.resulting_view,
            ChangeId::from_str("02890f2d-3c4b-7a68-9abc-def012345678").unwrap(),
            [],
            edit("docs/guide.md", &payloads[2]),
            ApplicationContext::new(&content),
        )
        .unwrap();
    RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    Fixture {
        _root: root,
        objects,
        content,
        heads,
        repository,
        payloads,
    }
}

#[derive(Debug)]
struct CountingObjects<'a> {
    inner: &'a LooseObjectStore,
    reads: AtomicUsize,
}

#[async_trait]
impl wheat_store::ImmutableObjectStore for CountingObjects<'_> {
    async fn put_if_absent(
        &self,
        id: wheat_core::ObjectId,
        bytes: &[u8],
    ) -> Result<(), wheat_store::ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(
        &self,
        id: wheat_core::ObjectId,
    ) -> Result<StoredObject, wheat_store::ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get(id).await
    }

    async fn exists_many(
        &self,
        ids: &[wheat_core::ObjectId],
    ) -> Result<Vec<bool>, wheat_store::ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

async fn snapshot() -> (NativeSnapshot, Repository) {
    let fixture = fixture().await;
    let snapshot = export_snapshot(
        &fixture.objects,
        &fixture.content,
        &fixture.heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    assert!(snapshot.manifest.objects.len() >= 10);
    assert_eq!(snapshot.manifest.contents.len(), fixture.payloads.len());
    (snapshot, fixture.repository)
}

#[tokio::test]
async fn real_repository_round_trip_restores_exact_validated_closure_and_content() {
    let (snapshot, expected) = snapshot().await;
    assert_eq!(
        SnapshotManifest::decode(
            &snapshot.manifest.canonical_bytes(),
            SnapshotLimits::default()
        )
        .unwrap(),
        snapshot.manifest
    );
    let destination = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(destination.path());
    let restored = restore_snapshot(
        &snapshot,
        &objects,
        &content,
        &heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    assert_eq!(restored, snapshot.manifest.head);
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_envelope(restored.envelope).await.unwrap();
    let envelope = reader.envelope(restored.envelope).await.unwrap();
    let actual = reader.hydrate(envelope.state).await.unwrap();
    assert_eq!(
        actual.canonical_closure().state.id,
        expected.canonical_closure().state.id
    );
    for entry in &snapshot.manifest.contents {
        let bytes = content.get_bounded(entry.id, entry.bytes as usize).unwrap();
        assert_eq!(ContentId::of(&bytes), entry.id);
    }
    assert_eq!(
        restore_snapshot(
            &snapshot,
            &objects,
            &content,
            &heads,
            SnapshotLimits::default()
        )
        .await
        .unwrap(),
        restored
    );
}

#[tokio::test]
async fn same_head_restore_reauthenticates_and_repairs_missing_payloads() {
    let (snapshot, _) = snapshot().await;
    let destination = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(destination.path());
    restore_snapshot(
        &snapshot,
        &objects,
        &content,
        &heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    let missing_object = snapshot.manifest.objects[0].id;
    let missing_content = snapshot.manifest.contents[0].id;
    std::fs::remove_file(objects.path(missing_object)).unwrap();
    std::fs::remove_file(content.path(missing_content)).unwrap();

    restore_snapshot(
        &snapshot,
        &objects,
        &content,
        &heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    assert!(objects.get(missing_object).await.is_ok());
    assert!(content.get(missing_content).is_ok());

    let mut corrupt = snapshot.clone();
    corrupt.objects[0].1.push(0);
    assert!(matches!(
        restore_snapshot(
            &corrupt,
            &objects,
            &content,
            &heads,
            SnapshotLimits::default()
        )
        .await,
        Err(SnapshotError::InventoryMismatch)
    ));
}

#[tokio::test]
async fn exact_restored_closure_rejects_manifest_extras_and_preexisting_omissions() {
    let (snapshot, _) = snapshot().await;
    let destination = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(destination.path());
    restore_snapshot(
        &snapshot,
        &objects,
        &content,
        &heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();

    let extra_node = wheat_core::DirectoryNode::default();
    let extra_bytes = extra_node.canonical_bytes();
    let extra_id: wheat_core::ObjectId = extra_node.id().into();
    let mut extra = snapshot.clone();
    let position = extra
        .manifest
        .objects
        .binary_search_by_key(&extra_id, |entry| entry.id)
        .unwrap_err();
    extra.manifest.objects.insert(
        position,
        InventoryEntry {
            id: extra_id,
            bytes: extra_bytes.len() as u64,
        },
    );
    extra.objects.insert(position, (extra_id, extra_bytes));
    extra.manifest_id = extra.manifest.id();
    assert!(matches!(
        restore_snapshot(
            &extra,
            &objects,
            &content,
            &heads,
            SnapshotLimits::default()
        )
        .await,
        Err(SnapshotError::InventoryMismatch)
    ));

    let mut omitted = snapshot.clone();
    omitted.manifest.objects.remove(0);
    omitted.objects.remove(0);
    omitted.manifest_id = omitted.manifest.id();
    assert!(matches!(
        restore_snapshot(
            &omitted,
            &objects,
            &content,
            &heads,
            SnapshotLimits::default()
        )
        .await,
        Err(SnapshotError::InventoryMismatch)
    ));
}

#[tokio::test]
async fn corrupt_and_missing_object_and_content_each_fail_before_publication() {
    let (original, _) = snapshot().await;
    for mutation in 0..4 {
        let mut candidate = original.clone();
        match mutation {
            0 => candidate.objects[0].1.push(0),
            1 => {
                candidate.objects.remove(0);
            }
            2 => candidate.contents[0].1.push(0),
            _ => {
                candidate.contents.remove(0);
            }
        }
        let destination = tempfile::tempdir().unwrap();
        let (objects, content, heads) = stores(destination.path());
        assert!(
            restore_snapshot(
                &candidate,
                &objects,
                &content,
                &heads,
                SnapshotLimits::default()
            )
            .await
            .is_err()
        );
        assert!(
            heads.read().is_err(),
            "mutation {mutation} published a head"
        );
    }
}

#[derive(Default)]
struct RecordingSink {
    memory: MemorySnapshotSink,
    calls: Mutex<Vec<(char, usize, usize)>>,
    fail_payload_call: AtomicUsize,
    payload_calls: AtomicUsize,
    manifest_calls: AtomicUsize,
}

impl RecordingSink {
    fn fail_once_at(call: usize) -> Self {
        Self {
            fail_payload_call: AtomicUsize::new(call),
            ..Self::default()
        }
    }
}

#[async_trait]
impl SnapshotSink for RecordingSink {
    async fn write_objects(
        &self,
        batch: &[(wheat_core::ObjectId, Vec<u8>)],
    ) -> Result<(), SnapshotError> {
        self.record(
            'o',
            batch.len(),
            batch.iter().map(|item| item.1.len()).sum(),
        )?;
        self.memory.write_objects(batch).await
    }

    async fn write_contents(&self, batch: &[(ContentId, Vec<u8>)]) -> Result<(), SnapshotError> {
        self.record(
            'c',
            batch.len(),
            batch.iter().map(|item| item.1.len()).sum(),
        )?;
        self.memory.write_contents(batch).await
    }

    async fn commit_manifest(&self, id: SnapshotId, bytes: &[u8]) -> Result<(), SnapshotError> {
        self.manifest_calls.fetch_add(1, Ordering::SeqCst);
        self.memory.commit_manifest(id, bytes).await
    }
}

impl RecordingSink {
    fn record(&self, kind: char, items: usize, bytes: usize) -> Result<(), SnapshotError> {
        let call = self.payload_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.calls.lock().unwrap().push((kind, items, bytes));
        if self
            .fail_payload_call
            .compare_exchange(call, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(SnapshotError::InventoryMismatch);
        }
        Ok(())
    }
}

#[tokio::test]
async fn tiny_export_count_limit_stops_lazy_traversal_before_repository_walk() {
    let fixture = fixture().await;
    let counted = CountingObjects {
        inner: &fixture.objects,
        reads: AtomicUsize::new(0),
    };
    let result = export_snapshot(
        &counted,
        &fixture.content,
        &fixture.heads,
        SnapshotLimits {
            max_objects: 2,
            ..SnapshotLimits::default()
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(SnapshotError::LimitExceeded("inventory count"))
    ));
    assert!(
        counted.reads.load(Ordering::SeqCst) <= 2,
        "tiny bound must reject before traversing retained history"
    );
}

#[tokio::test]
async fn export_is_exactly_bounded_manifest_last_and_interruption_is_retryable() {
    let fixture = fixture().await;
    let inventory = export_snapshot(
        &fixture.objects,
        &fixture.content,
        &fixture.heads,
        SnapshotLimits::default(),
    )
    .await
    .unwrap();
    let largest_item = inventory
        .manifest
        .objects
        .iter()
        .map(|entry| entry.bytes)
        .chain(inventory.manifest.contents.iter().map(|entry| entry.bytes))
        .max()
        .unwrap() as usize;
    let limits = SnapshotLimits {
        batch_items: 2,
        batch_bytes: largest_item,
        ..SnapshotLimits::default()
    };
    let sink = RecordingSink::fail_once_at(2);
    assert!(
        export_snapshot_to(
            &fixture.objects,
            &fixture.content,
            &fixture.heads,
            &sink,
            limits
        )
        .await
        .is_err()
    );
    assert_eq!(sink.manifest_calls.load(Ordering::SeqCst), 0);

    let id = export_snapshot_to(
        &fixture.objects,
        &fixture.content,
        &fixture.heads,
        &sink,
        limits,
    )
    .await
    .unwrap();
    assert_eq!(sink.manifest_calls.load(Ordering::SeqCst), 1);
    {
        let calls = sink.calls.lock().unwrap();
        assert!(calls.iter().all(|(_, items, bytes)| {
            *items <= limits.batch_items && *bytes <= limits.batch_bytes
        }));
        assert!(
            calls
                .iter()
                .any(|(_, items, _)| *items == limits.batch_items)
        );
        assert!(
            calls
                .iter()
                .any(|(_, _, bytes)| *bytes == limits.batch_bytes)
        );
    }
    let (manifest_id, _) = sink
        .memory
        .into_snapshot(limits)
        .unwrap()
        .read_manifest()
        .await
        .unwrap();
    assert_eq!(manifest_id, id);
}

struct CheckedSource {
    snapshot: NativeSnapshot,
    limits: SnapshotLimits,
    calls: AtomicUsize,
    fail_once_at: AtomicUsize,
    saw_exact_item_limit: AtomicBool,
}

#[async_trait]
impl SnapshotSource for CheckedSource {
    async fn read_manifest(&self) -> Result<(SnapshotId, Vec<u8>), SnapshotError> {
        self.snapshot.read_manifest().await
    }

    async fn read_objects(
        &self,
        inventory: &[InventoryEntry<wheat_core::ObjectId>],
    ) -> Result<ObjectBatch, SnapshotError> {
        self.check(
            inventory.iter().map(|item| item.bytes as usize).sum(),
            inventory.len(),
        )?;
        self.snapshot.read_objects(inventory).await
    }

    async fn read_contents(
        &self,
        inventory: &[InventoryEntry<ContentId>],
    ) -> Result<ContentBatch, SnapshotError> {
        self.check(
            inventory.iter().map(|item| item.bytes as usize).sum(),
            inventory.len(),
        )?;
        self.snapshot.read_contents(inventory).await
    }
}

impl CheckedSource {
    fn check(&self, bytes: usize, items: usize) -> Result<(), SnapshotError> {
        assert!(bytes <= self.limits.batch_bytes);
        assert!(items <= self.limits.batch_items);
        if items == self.limits.batch_items {
            self.saw_exact_item_limit.store(true, Ordering::SeqCst);
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self
            .fail_once_at
            .compare_exchange(call, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Err(SnapshotError::InventoryMismatch);
        }
        Ok(())
    }
}

#[tokio::test]
async fn streaming_restore_bounds_batches_and_retries_after_middle_source_failure() {
    let (snapshot, _) = snapshot().await;
    let limits = SnapshotLimits {
        batch_items: 2,
        batch_bytes: 1024,
        ..SnapshotLimits::default()
    };
    let source = CheckedSource {
        snapshot,
        limits,
        calls: AtomicUsize::new(0),
        fail_once_at: AtomicUsize::new(2),
        saw_exact_item_limit: AtomicBool::new(false),
    };
    let destination = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(destination.path());
    assert!(
        restore_snapshot_from(&source, &objects, &content, &heads, limits)
            .await
            .is_err()
    );
    assert!(heads.read().is_err());
    restore_snapshot_from(&source, &objects, &content, &heads, limits)
        .await
        .unwrap();
    assert!(source.saw_exact_item_limit.load(Ordering::SeqCst));
}

#[tokio::test]
async fn repository_and_snapshot_authorities_keep_shared_real_closure_live() {
    let (snapshot, _) = snapshot().await;
    let repository = snapshot.manifest.repository_ownership([42; 32]);
    let retained_snapshot = snapshot.manifest.snapshot_ownership();
    let proof = prove_union_liveness([
        (&repository, repository.authenticated_id()),
        (&retained_snapshot, retained_snapshot.authenticated_id()),
    ])
    .unwrap();
    assert_eq!(proof.objects.len(), snapshot.manifest.objects.len());
    assert_eq!(proof.contents.len(), snapshot.manifest.contents.len());
    assert!(
        snapshot
            .manifest
            .objects
            .iter()
            .all(|entry| proof.objects.contains(&entry.id))
    );
    assert!(
        snapshot
            .manifest
            .contents
            .iter()
            .all(|entry| proof.contents.contains(&entry.id))
    );
}

#[tokio::test]
async fn limits_and_conflicting_authority_fail_closed() {
    let (snapshot, _) = snapshot().await;
    for limits in [
        SnapshotLimits {
            max_objects: snapshot.manifest.objects.len() - 1,
            ..SnapshotLimits::default()
        },
        SnapshotLimits {
            max_contents: snapshot.manifest.contents.len() - 1,
            ..SnapshotLimits::default()
        },
        SnapshotLimits {
            max_total_bytes: 1,
            ..SnapshotLimits::default()
        },
        SnapshotLimits {
            max_manifest_bytes: 98,
            ..SnapshotLimits::default()
        },
    ] {
        let destination = tempfile::tempdir().unwrap();
        let (objects, content, heads) = stores(destination.path());
        assert!(matches!(
            restore_snapshot(&snapshot, &objects, &content, &heads, limits).await,
            Err(SnapshotError::LimitExceeded(_))
        ));
        assert!(heads.read().is_err());
    }

    let destination = tempfile::tempdir().unwrap();
    let (objects, content, heads) = stores(destination.path());
    let conflicting = wheat_store::RepositoryHead::initial(
        wheat_core::RepositoryEnvelopeId::from_digest([7; 32]),
    );
    heads.initialize(conflicting).unwrap();
    assert!(matches!(
        restore_snapshot(
            &snapshot,
            &objects,
            &content,
            &heads,
            SnapshotLimits::default()
        )
        .await,
        Err(SnapshotError::ConflictingAuthority)
    ));
    assert_eq!(heads.read().unwrap(), conflicting);
}
