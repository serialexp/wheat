//! Joining two envelopes that grew apart from shared history.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use wheat_core::*;
use wheat_store::*;

fn exact(path: &str, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(replacement),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

struct CountingStore<S> {
    inner: S,
    reads: AtomicUsize,
}

#[async_trait]
impl<S: ImmutableObjectStore> ImmutableObjectStore for CountingStore<S> {
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
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

struct Stores {
    dir: tempfile::TempDir,
    objects: CountingStore<LooseObjectStore>,
    content: ContentStore,
    /// Change ids come from a counter, so the maps keyed by them have the same shape on
    /// every run. Like UUIDv7 ids they grow with creation order, which is the canonical
    /// order of changes without dependencies.
    changes: AtomicUsize,
}

impl Stores {
    fn next_change(&self) -> ChangeId {
        let count = self.changes.fetch_add(1, Ordering::Relaxed) as u128 + 1;
        let tail = count.wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835) >> 48;
        ChangeId::from_bytes(((count << 80) | tail).to_be_bytes())
    }

    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects = CountingStore {
            inner: LooseObjectStore::with_durability(
                dir.path().join("objects"),
                Durability::Deferred,
            ),
            reads: AtomicUsize::new(0),
        };
        let content = ContentStore::new(dir.path().join("content"));
        Self {
            dir,
            objects,
            content,
            changes: AtomicUsize::new(0),
        }
    }

    fn reader(&self) -> RepositoryReader<'_, CountingStore<LooseObjectStore>> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    fn put(&self, bytes: &[u8]) {
        self.content.put(bytes).unwrap();
    }

    async fn record(&self, repository: &Repository, labels: &[(&str, ViewId)]) -> Envelope {
        let heads = LocalPublicationStore::new(self.dir.path().join("unused.head"));
        let publisher = RepositoryPublisher::new(&self.objects, &self.content, &heads);
        let closure = publisher.prepare(repository).await.unwrap();
        let labels = ViewLabelManifestRecord::new(labels.iter().copied()).unwrap();
        let id = publisher
            .prepare_envelope(closure.state.id, &labels)
            .await
            .unwrap();
        Envelope {
            id,
            state: closure.state.id,
        }
    }

    fn change(&self, repository: &mut Repository, on: ViewId, path: &str) -> (ChangeId, ViewId) {
        let bytes = format!("{path} contents");
        self.put(bytes.as_bytes());
        let change = self.next_change();
        let outcome = repository
            .create_change_with_id(
                on,
                on,
                change,
                [],
                exact(path, bytes.as_bytes()),
                ApplicationContext::new(&self.content),
            )
            .unwrap();
        (change, outcome.resulting_view)
    }
}

#[derive(Clone, Copy)]
struct Envelope {
    id: RepositoryEnvelopeId,
    state: RepositoryStateId,
}

/// A shared history of `shared` changes on `main`, then one change on each side: the
/// local one moves `main`, the remote one adds a `feature` label.
struct Diverged {
    stores: Stores,
    base: Envelope,
    local: Envelope,
    remote: Envelope,
    /// Both sides' work recorded in one repository.
    both: Envelope,
    local_view: ViewId,
    remote_view: ViewId,
}

async fn diverged(shared: usize) -> Diverged {
    let stores = Stores::new();
    let mut repository = Repository::new(Tree::new());
    let mut main = repository.base_view().id;
    for index in 0..shared {
        main = stores
            .change(&mut repository, main, &format!("shared/{index}"))
            .1;
    }
    let base = stores.record(&repository, &[("main", main)]).await;

    let mut local = repository.clone();
    let (_, local_view) = stores.change(&mut local, main, "local");
    let mut remote = repository;
    let (remote_change, remote_view) = stores.change(&mut remote, main, "remote");

    // The same two changes, made one after the other in one repository.
    let mut both = local.clone();
    both.create_change_with_id(
        main,
        main,
        remote_change,
        [],
        exact("remote", b"remote contents"),
        ApplicationContext::new(&stores.content),
    )
    .unwrap();

    let local = stores.record(&local, &[("main", local_view)]).await;
    let remote = stores
        .record(&remote, &[("main", main), ("feature", remote_view)])
        .await;
    let both = stores
        .record(&both, &[("main", local_view), ("feature", remote_view)])
        .await;
    Diverged {
        stores,
        base,
        local,
        remote,
        both,
        local_view,
        remote_view,
    }
}

#[tokio::test]
async fn the_union_is_the_history_both_sides_made() {
    let diverged = diverged(3).await;
    let reader = diverged.stores.reader();

    let union = reader
        .union_envelopes(
            Some(diverged.base.id),
            diverged.local.id,
            diverged.remote.id,
        )
        .await
        .unwrap();

    assert_eq!(union, diverged.both.id);
    let envelope = reader.validate_envelope(union).await.unwrap();
    let labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    assert_eq!(labels.labels["main"].view, diverged.local_view);
    assert_eq!(labels.labels["feature"].view, diverged.remote_view);
    // A valid successor of both sides, so either one can publish it.
    for side in [diverged.local.state, diverged.remote.state] {
        reader
            .validate_transition(side, envelope.state)
            .await
            .unwrap();
    }
    // The order of the sides does not change the state.
    assert_eq!(
        reader
            .union_states(diverged.remote.state, diverged.local.state)
            .await
            .unwrap(),
        envelope.state
    );
}

#[tokio::test]
async fn a_side_that_did_not_move_takes_the_other() {
    let diverged = diverged(1).await;
    let reader = diverged.stores.reader();
    let base = Some(diverged.base.id);

    let pulled = reader
        .union_envelopes(base, diverged.base.id, diverged.remote.id)
        .await
        .unwrap();
    let kept = reader
        .union_envelopes(base, diverged.local.id, diverged.base.id)
        .await
        .unwrap();

    assert_eq!(pulled, diverged.remote.id);
    assert_eq!(kept, diverged.local.id);
}

#[tokio::test]
async fn a_change_recorded_differently_on_each_side_is_refused() {
    let stores = Stores::new();
    stores.put(b"x");
    let repository = Repository::new(Tree::new());
    let base_view = repository.base_view().id;
    let base = stores.record(&repository, &[("main", base_view)]).await;
    let change = ChangeId::new();
    let mut sides = Vec::new();
    for title in ["here", "there"] {
        let mut side = repository.clone();
        side.create_change_with_metadata(
            base_view,
            base_view,
            change,
            ChangeMetadata {
                title: title.into(),
                ..ChangeMetadata::default()
            },
            [],
            RevisionMetadata::default(),
            exact("x", b"x"),
            ApplicationContext::new(&stores.content),
        )
        .unwrap();
        sides.push(stores.record(&side, &[("main", base_view)]).await);
    }

    let error = stores
        .reader()
        .union_envelopes(Some(base.id), sides[0].id, sides[1].id)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RepositoryUnionError::DivergentChanges(changes) if changes == &[change]),
        "{error}"
    );
}

#[tokio::test]
async fn repositories_from_different_bases_are_refused() {
    let stores = Stores::new();
    stores.put(b"seed");
    let empty = Repository::new(Tree::new());
    let seeded = Repository::new(Tree::from([(
        "seed".into(),
        FileValue {
            content: ContentId::of(b"seed"),
            kind: FileKind::Regular,
        },
    )]));
    let here = stores
        .record(&empty, &[("main", empty.base_view().id)])
        .await;
    let there = stores
        .record(&seeded, &[("main", seeded.base_view().id)])
        .await;

    let error = stores
        .reader()
        .union_states(here.state, there.state)
        .await
        .unwrap_err();

    assert!(
        matches!(error, RepositoryUnionError::DifferentBaseView { .. }),
        "{error}"
    );
}

#[tokio::test]
async fn a_label_both_sides_moved_differently_is_refused() {
    let diverged = diverged(1).await;
    let reader = diverged.stores.reader();
    // The remote moved `main` too, somewhere else than the local side did.
    let remote = reader.envelope(diverged.remote.id).await.unwrap();
    let labels = ViewLabelManifestRecord::new([("main", diverged.remote_view)]).unwrap();
    let heads = LocalPublicationStore::new(diverged.stores.dir.path().join("unused.head"));
    let moved =
        RepositoryPublisher::new(&diverged.stores.objects, &diverged.stores.content, &heads)
            .prepare_envelope(remote.state, &labels)
            .await
            .unwrap();

    let error = reader
        .union_envelopes(Some(diverged.base.id), diverged.local.id, moved)
        .await
        .unwrap_err();

    assert!(
        matches!(&error, RepositoryUnionError::DivergentLabels(names) if names == &["main"]),
        "{error}"
    );
}

/// Each side within the label cap, but not the two together: refused.
#[tokio::test]
async fn a_union_over_the_label_cap_is_refused() {
    let diverged = diverged(1).await;
    let reader = diverged.stores.reader();
    let heads = LocalPublicationStore::new(diverged.stores.dir.path().join("unused.head"));
    let publisher =
        RepositoryPublisher::new(&diverged.stores.objects, &diverged.stores.content, &heads);
    let mut sides = Vec::new();
    for (side, view) in [
        (diverged.local, diverged.local_view),
        (diverged.remote, diverged.remote_view),
    ] {
        let labels = ViewLabelManifestRecord::new(
            (0..=MAX_LABELS / 2).map(|index| (format!("{view}/{index}"), view)),
        )
        .unwrap();
        sides.push(
            publisher
                .prepare_envelope(side.state, &labels)
                .await
                .unwrap(),
        );
    }

    let error = reader
        .union_envelopes(Some(diverged.base.id), sides[0], sides[1])
        .await
        .unwrap_err();

    assert!(
        matches!(error, RepositoryUnionError::TooManyLabels(count) if count > MAX_LABELS),
        "{error}"
    );
}

#[test]
fn labels_merge_three_ways() {
    let view = |n: u8| ViewId::from_digest([n; 32]);
    let manifest = |labels: &[(&str, u8)]| {
        ViewLabelManifestRecord::new(labels.iter().map(|(name, n)| (*name, view(*n)))).unwrap()
    };
    let base = manifest(&[
        ("kept", 1),
        ("here", 1),
        ("there", 1),
        ("same", 1),
        ("gone", 1),
    ]);
    let local = manifest(&[
        ("kept", 1),
        ("here", 2),
        ("there", 1),
        ("same", 3),
        ("gone", 1),
        ("new", 4),
    ]);
    let remote = manifest(&[("kept", 1), ("here", 1), ("there", 2), ("same", 3)]);

    let merged = merge_labels(&base, &local, &remote).unwrap();

    assert_eq!(
        merged,
        manifest(&[
            ("kept", 1),
            ("here", 2),
            ("there", 2),
            ("same", 3),
            ("new", 4),
        ])
    );

    // Removed on one side and moved on the other, or created on both with
    // different targets: both conflict.
    let local = manifest(&[("gone", 2), ("fresh", 1)]);
    let remote = manifest(&[("fresh", 2)]);
    assert_eq!(
        merge_labels(&base, &local, &remote).unwrap_err(),
        ["fresh", "gone"]
    );
}

/// Joining one change on each side reads what the sides differ by, so a longer shared
/// history adds only the few radix levels it deepens.
#[tokio::test]
async fn the_union_reads_what_the_sides_differ_by() {
    let mut reads = Vec::new();
    for shared in [16, 160] {
        let diverged = diverged(shared).await;
        diverged.stores.objects.reads.store(0, Ordering::Relaxed);
        diverged
            .stores
            .reader()
            .union_states(diverged.local.state, diverged.remote.state)
            .await
            .unwrap();
        reads.push(diverged.stores.objects.reads.load(Ordering::Relaxed));
    }
    assert!(
        reads[1] <= reads[0] + 16,
        "reads grew with the shared history: {reads:?}"
    );
}
