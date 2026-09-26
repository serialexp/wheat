//! `StructuralDelta` against the reference model's closures.
//!
//! For envelopes `old` and `new` the walk must yield every object of closure(new)
//! that is not in closure(old), and nothing outside closure(new).

use std::collections::HashSet;

use async_trait::async_trait;
use wheat_core::*;
use wheat_store::*;

fn exact(path: &str, expected: Option<&[u8]>, replacement: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
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

/// Nothing here outlives the test, so skip the per-object fsync.
fn scratch_store(dir: &tempfile::TempDir) -> LooseObjectStore {
    LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred)
}

struct History {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    /// One envelope per step of one history, oldest first.
    envelopes: Vec<RepositoryEnvelopeId>,
    closures: Vec<HashSet<ObjectId>>,
    /// An envelope of a history that shares nothing with the other one.
    unrelated: RepositoryEnvelopeId,
    unrelated_closure: HashSet<ObjectId>,
}

async fn record(
    objects: &LooseObjectStore,
    content: &ContentStore,
    repository: &Repository,
    labels: &[(&str, ViewId)],
) -> (RepositoryEnvelopeId, HashSet<ObjectId>) {
    let heads = LocalPublicationStore::new(tempfile::tempdir().unwrap().path().join("head"));
    let publisher = RepositoryPublisher::new(objects, content, &heads);
    let closure = publisher.prepare(repository).await.unwrap();
    let labels = ViewLabelManifestRecord::new(labels.iter().copied()).unwrap();
    let envelope = publisher
        .prepare_envelope(closure.state.id, &labels)
        .await
        .unwrap();
    let mut ids: HashSet<ObjectId> = closure.objects.iter().map(|object| object.id).collect();
    ids.insert(envelope.into());
    ids.insert(labels.id().into());
    (envelope, ids)
}

/// Branches, a dependency, a revision that conflicts a later change, its
/// resolution, and labels that move.
async fn history() -> History {
    let dir = tempfile::tempdir().unwrap();
    let objects = scratch_store(&dir);
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [
        b"a1".as_slice(),
        b"a2",
        b"b1",
        b"c1",
        b"d1",
        b"resolved",
        b"readme",
    ] {
        content.put(bytes).unwrap();
    }
    let mut envelopes = Vec::new();
    let mut closures = Vec::new();
    macro_rules! step {
        ($repository:expr, $labels:expr) => {{
            let (envelope, closure) = record(&objects, &content, &$repository, $labels).await;
            envelopes.push(envelope);
            closures.push(closure);
        }};
    }

    let mut repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    step!(repository, &[("main", base)]);

    let a = ChangeId::new();
    let a1_edits = exact("a", None, b"a1");
    let a1_edit = *a1_edits.edits.keys().next().unwrap();
    let a1 = repository
        .create_change_with_id(
            base,
            base,
            a,
            [],
            a1_edits,
            ApplicationContext::new(&content),
        )
        .unwrap();
    step!(repository, &[("main", a1.resulting_view)]);

    let b = ChangeId::new();
    let b1 = repository
        .create_change_with_id(
            base,
            a1.resulting_view,
            b,
            [],
            exact("b", None, b"b1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    step!(repository, &[("main", b1.resulting_view)]);

    // A second branch off the base view.
    let d = ChangeId::new();
    let d1 = repository
        .create_change_with_id(
            base,
            base,
            d,
            [],
            exact("d", None, b"d1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    step!(
        repository,
        &[("main", b1.resulting_view), ("side", d1.resulting_view)]
    );

    let c = ChangeId::new();
    let c1 = repository
        .create_change_with_metadata(
            base,
            b1.resulting_view,
            c,
            ChangeMetadata::default(),
            [a, b],
            RevisionMetadata::default(),
            exact("a", Some(b"a1"), b"c1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    step!(
        repository,
        &[("main", c1.resulting_view), ("side", d1.resulting_view)]
    );

    // Revising `a` conflicts `c`, which expected the old value.
    let mut a2_edits = exact("a", None, b"a2");
    a2_edits.transition.removed.insert(a1_edit);
    let a2 = repository
        .revise_change(
            base,
            c1.resulting_view,
            a,
            a1.revision_id,
            a2_edits,
            ApplicationContext::new(&content),
        )
        .unwrap();
    let conflicted_view = a2.resulting_view;
    assert!(
        !repository
            .view(conflicted_view)
            .unwrap()
            .unresolved_conflicts
            .is_empty()
    );
    step!(
        repository,
        &[("main", conflicted_view), ("side", d1.resulting_view)]
    );

    let conflict_revision = repository.current_revision(conflicted_view, c).unwrap();
    let conflicted = repository
        .conflicted_application(conflicted_view, conflict_revision)
        .unwrap()
        .clone();
    let mut resolved_tree = conflicted.result.materialized_tree().clone();
    resolved_tree.insert(
        "a".into(),
        FileValue {
            content: ContentId::of(b"resolved"),
            kind: FileKind::Regular,
        },
    );
    let resolution = repository
        .resolve_conflict(
            base,
            conflicted_view,
            ConflictResolution {
                conflict_revision,
                resolved_tree,
            },
            ApplicationContext::new(&content),
        )
        .unwrap();
    step!(
        repository,
        &[
            ("main", resolution.resulting_view),
            ("side", d1.resulting_view),
            ("archive", base),
        ]
    );

    let mut readme = Tree::new();
    readme.insert(
        "README".into(),
        FileValue {
            content: ContentId::of(b"readme"),
            kind: FileKind::Regular,
        },
    );
    let other = Repository::new(readme);
    let other_base = other.base_view().id;
    let (unrelated, unrelated_closure) =
        record(&objects, &content, &other, &[("main", other_base)]).await;

    History {
        _dir: dir,
        objects,
        content,
        envelopes,
        closures,
        unrelated,
        unrelated_closure,
    }
}

/// Runs a walk that must not need anything, returning what it yielded.
async fn walk<S: ImmutableObjectStore>(
    objects: &S,
    old: Option<RepositoryEnvelopeId>,
    new: RepositoryEnvelopeId,
) -> HashSet<ObjectId> {
    let mut delta = StructuralDelta::new(objects, old, new, StructuralDeltaLimits::default());
    let mut yielded = HashSet::new();
    while let Some(step) = delta.next().await.unwrap() {
        match step {
            DeltaStep::Object(object) => {
                assert!(yielded.insert(object.id), "{} yielded twice", object.id);
            }
            DeltaStep::Missing(ids) => panic!("walk needs {ids:?}"),
        }
    }
    assert_eq!(delta.stats().emitted, yielded.len() as u64);
    yielded
}

fn assert_between(
    yielded: &HashSet<ObjectId>,
    old: &HashSet<ObjectId>,
    new: &HashSet<ObjectId>,
    what: &str,
) {
    let outside: Vec<_> = yielded.difference(new).collect();
    assert!(
        outside.is_empty(),
        "{what}: yielded outside the closure: {outside:?}"
    );
    let skipped: Vec<_> = new
        .difference(old)
        .filter(|id| !yielded.contains(id))
        .collect();
    assert!(
        skipped.is_empty(),
        "{what}: skipped new objects: {skipped:?}"
    );
}

#[tokio::test]
async fn from_nothing_yields_exactly_the_closure() {
    let history = history().await;
    for (index, (envelope, closure)) in history
        .envelopes
        .iter()
        .zip(&history.closures)
        .chain([(&history.unrelated, &history.unrelated_closure)])
        .enumerate()
    {
        let yielded = walk(&history.objects, None, *envelope).await;
        assert_eq!(&yielded, closure, "envelope {index}");
    }
}

/// The objects a snapshot export of `envelope` lists.
async fn snapshot_objects(
    objects: &LooseObjectStore,
    content: &ContentStore,
    envelope: RepositoryEnvelopeId,
) -> HashSet<ObjectId> {
    let dir = tempfile::tempdir().unwrap();
    let publications = LocalPublicationStore::new(dir.path().join("head"));
    publications
        .initialize(RepositoryHead::initial(envelope))
        .unwrap();
    let snapshot = export_snapshot(objects, content, &publications, SnapshotLimits::default())
        .await
        .unwrap();
    snapshot
        .manifest
        .objects
        .iter()
        .map(|entry| entry.id)
        .collect()
}

/// Snapshot export and the walk get their references from one place, so both find
/// the whole closure.
#[tokio::test]
async fn a_snapshot_export_lists_the_same_closure_as_the_walk() {
    let history = history().await;
    for (index, (envelope, closure)) in history
        .envelopes
        .iter()
        .zip(&history.closures)
        .chain([(&history.unrelated, &history.unrelated_closure)])
        .enumerate()
    {
        let listed = snapshot_objects(&history.objects, &history.content, *envelope).await;
        assert_eq!(&listed, closure, "envelope {index}");
    }
}

#[tokio::test]
async fn every_pair_yields_the_difference_and_nothing_outside_the_closure() {
    let history = history().await;
    let all: Vec<_> = history
        .envelopes
        .iter()
        .zip(&history.closures)
        .chain([(&history.unrelated, &history.unrelated_closure)])
        .collect();
    for (i, (old, old_closure)) in all.iter().enumerate() {
        for (j, (new, new_closure)) in all.iter().enumerate() {
            let yielded = walk(&history.objects, Some(**old), **new).await;
            assert_between(&yielded, old_closure, new_closure, &format!("{i} -> {j}"));
            if i == j {
                assert!(yielded.is_empty());
            }
        }
    }
}

#[tokio::test]
async fn one_step_forward_yields_little_more_than_the_difference() {
    let history = history().await;
    for i in 1..history.envelopes.len() {
        let yielded = walk(
            &history.objects,
            Some(history.envelopes[i - 1]),
            history.envelopes[i],
        )
        .await;
        // Everything pairs up here, so the walk should yield exactly what is new, except
        // that an emptied collection's one-node root can't be told old from its pair.
        let empty_roots = empty_roots(&history.objects).await;
        let mut extra = Vec::new();
        for id in yielded.intersection(&history.closures[i - 1]) {
            if !empty_roots.contains(id) {
                extra.push(history.objects.get(*id).await.unwrap().kind);
            }
        }
        assert!(
            extra.is_empty(),
            "step {i}: yielded {} of a closure of {}, including old {extra:?}",
            yielded.len(),
            history.closures[i].len()
        );
    }
}

/// The one-node roots of the empty collections.
async fn empty_roots(objects: &LooseObjectStore) -> HashSet<ObjectId> {
    let conflicts = ConflictSetStore::new(objects).empty_root().await.unwrap();
    let retained = RetainedSetStore::new(objects).empty_root().await.unwrap();
    let revisions = RevisionMapStore::new(objects).empty_root().await.unwrap();
    let applications = ApplicationMapStore::new(objects)
        .empty_root()
        .await
        .unwrap();
    let changes = ChangeMapStore::new(objects).empty_root().await.unwrap();
    let lineage = ViewLineageMapStore::new(objects)
        .empty_root()
        .await
        .unwrap();
    [
        conflicts.into(),
        retained.into(),
        revisions.into(),
        applications.into(),
        changes.into(),
        lineage.into(),
    ]
    .into()
}

/// Enough changes that every collection is a real radix tree: a main line, plus a
/// branch off the base view every third change, so the base view has a wide
/// lineage child set.
#[tokio::test]
async fn a_long_history_pairs_radix_trees_node_by_node() {
    const CHANGES: usize = 150;
    let recorded_at = [0, 60, 61, 62, 120, 149, 150];
    let dir = tempfile::tempdir().unwrap();
    let objects = scratch_store(&dir);
    let content = ContentStore::new(dir.path().join("content"));
    let mut repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut main = base;
    let mut recorded = Vec::new();
    for step in 0..=CHANGES {
        if step > 0 {
            let value = format!("value {step}");
            content.put(value.as_bytes()).unwrap();
            let effective = if step % 3 == 0 { base } else { main };
            let outcome = repository
                .create_change_with_id(
                    base,
                    effective,
                    ChangeId::new(),
                    [],
                    exact(
                        &format!("dir{}/file{step}", step % 7),
                        None,
                        value.as_bytes(),
                    ),
                    ApplicationContext::new(&content),
                )
                .unwrap();
            if effective == main {
                main = outcome.resulting_view;
            }
        }
        if recorded_at.contains(&step) {
            recorded.push(record(&objects, &content, &repository, &[("main", main)]).await);
        }
    }

    for (i, (old, old_closure)) in recorded.iter().enumerate() {
        for (j, (new, new_closure)) in recorded.iter().enumerate() {
            let yielded = walk(&objects, Some(*old), *new).await;
            assert_between(&yielded, old_closure, new_closure, &format!("{i} -> {j}"));
            if recorded_at[j] == recorded_at[i] + 1 {
                // One change: all of the old side pairs, nothing old comes back.
                let old_again: Vec<_> = yielded.intersection(old_closure).collect();
                assert!(old_again.is_empty(), "{i} -> {j}: {old_again:?}");
            }
        }
    }
    let (last, last_closure) = recorded.last().unwrap();
    assert_eq!(&walk(&objects, None, *last).await, last_closure);
    // Big enough for sets with members stored inline in their nodes.
    let listed = snapshot_objects(&objects, &content, *last).await;
    let mut left_out = Vec::new();
    for id in last_closure.difference(&listed) {
        left_out.push(objects.get(*id).await.unwrap().kind);
    }
    assert!(left_out.is_empty(), "snapshot left out {left_out:?}");
    assert!(listed.is_subset(last_closure));
}

/// Answers `Missing` for some IDs, as a store that lacks them would.
struct Hiding<'a> {
    inner: &'a LooseObjectStore,
    hidden: HashSet<ObjectId>,
}

#[async_trait]
impl ImmutableObjectStore for Hiding<'_> {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        if self.hidden.contains(&id) {
            return Err(ObjectStoreError::Missing(id));
        }
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        let mut found = self.inner.exists_many(ids).await?;
        for (id, found) in ids.iter().zip(&mut found) {
            *found &= !self.hidden.contains(id);
        }
        Ok(found)
    }
}

#[tokio::test]
async fn a_missing_old_side_only_makes_the_walk_yield_more() {
    let history = history().await;
    let last = history.envelopes.len() - 1;
    let (to, new) = (history.envelopes[last], &history.closures[last]);
    for old in 0..last {
        let from = history.envelopes[old];
        let old_only: Vec<_> = history.closures[old].difference(new).copied().collect();
        // Without the old envelope nothing pairs, so everything comes back.
        let store = Hiding {
            inner: &history.objects,
            hidden: old_only.iter().copied().collect(),
        };
        assert_eq!(&walk(&store, Some(from), to).await, new, "old {old} hidden");
        // With half of it gone, pairing is partial but still sound.
        let store = Hiding {
            inner: &history.objects,
            hidden: old_only
                .iter()
                .copied()
                .filter(|id| id.as_bytes()[0] % 2 == 0 && *id != from.into())
                .collect(),
        };
        let yielded = walk(&store, Some(from), to).await;
        let what = format!("old {old} half");
        assert_between(&yielded, &history.closures[old], new, &what);
    }
}

#[tokio::test]
async fn a_partial_store_is_completed_by_asking_for_what_is_missing() {
    let history = history().await;
    let last = history.envelopes.len() - 1;
    for old in 0..last {
        for keep_every in [1, 2, 3, usize::MAX] {
            let dir = tempfile::tempdir().unwrap();
            let target = scratch_store(&dir);
            let mut copy = history.closures[old].iter().copied().collect::<Vec<_>>();
            copy.extend(
                history.closures[last]
                    .difference(&history.closures[old])
                    .enumerate()
                    .filter(|(index, _)| keep_every != usize::MAX && index % keep_every == 0)
                    .map(|(_, id)| *id),
            );
            for id in copy {
                let object = history.objects.get(id).await.unwrap();
                target
                    .put_if_absent(id, &object.canonical_bytes)
                    .await
                    .unwrap();
            }

            let mut delta = StructuralDelta::new(
                &target,
                Some(history.envelopes[old]),
                history.envelopes[last],
                StructuralDeltaLimits::default(),
            );
            let mut fetched = 0;
            while let Some(step) = delta.next().await.unwrap() {
                let DeltaStep::Missing(ids) = step else {
                    continue;
                };
                assert!(!ids.is_empty());
                for id in ids {
                    assert!(history.closures[last].contains(&id), "asked for {id}");
                    let object = history.objects.get(id).await.unwrap();
                    target
                        .put_if_absent(id, &object.canonical_bytes)
                        .await
                        .unwrap();
                    fetched += 1;
                }
                delta.resume();
            }
            let complete = target
                .exists_many(&history.closures[last].iter().copied().collect::<Vec<_>>())
                .await
                .unwrap();
            assert!(
                complete.iter().all(|found| *found),
                "old {old}, keep every {keep_every}: incomplete after {fetched} fetched"
            );
            let reader = RepositoryReader::new(&target, &history.content);
            let envelope = reader.envelope(history.envelopes[last]).await.unwrap();
            reader.validate_complete(envelope.state).await.unwrap();
        }
    }
}

#[tokio::test]
async fn missing_ids_come_in_bounded_batches() {
    let history = history().await;
    let last = history.envelopes.len() - 1;
    let dir = tempfile::tempdir().unwrap();
    let target = scratch_store(&dir);
    let limits = StructuralDeltaLimits {
        max_missing_batch: 2,
        ..StructuralDeltaLimits::default()
    };
    let mut delta = StructuralDelta::new(&target, None, history.envelopes[last], limits);
    let mut asked = HashSet::new();
    while let Some(step) = delta.next().await.unwrap() {
        match step {
            DeltaStep::Missing(ids) => {
                assert!((1..=2).contains(&ids.len()));
                for id in ids {
                    assert!(asked.insert(id), "{id} asked for twice");
                    let object = history.objects.get(id).await.unwrap();
                    target
                        .put_if_absent(id, &object.canonical_bytes)
                        .await
                        .unwrap();
                }
                delta.resume();
            }
            DeltaStep::Object(_) => {}
        }
    }
    assert_eq!(asked, history.closures[last]);
}

#[tokio::test]
async fn an_envelope_id_that_is_not_an_envelope_is_refused() {
    let history = history().await;
    let reader = RepositoryReader::new(&history.objects, &history.content);
    let envelope = reader.envelope(history.envelopes[0]).await.unwrap();
    let not_an_envelope =
        RepositoryEnvelopeId::from_digest(*ObjectId::from(envelope.state).as_bytes());
    let mut delta = StructuralDelta::new(
        &history.objects,
        None,
        not_an_envelope,
        StructuralDeltaLimits::default(),
    );
    let error = delta.next().await.unwrap_err();
    assert!(
        matches!(
            error,
            StructuralDeltaError::WrongKind {
                actual: ObjectKind::RepositoryState,
                ..
            }
        ),
        "{error}"
    );
}

#[tokio::test]
async fn the_same_envelope_yields_nothing() {
    let history = history().await;
    let last = *history.envelopes.last().unwrap();
    let mut delta = StructuralDelta::new(
        &history.objects,
        Some(last),
        last,
        StructuralDeltaLimits::default(),
    );
    assert!(delta.next().await.unwrap().is_none());
    assert_eq!(delta.stats().new_reads, 0);
}
