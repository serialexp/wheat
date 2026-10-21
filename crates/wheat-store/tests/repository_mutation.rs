use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use wheat_core::*;
use wheat_store::*;

struct CountingStore {
    inner: LooseObjectStore,
    tree_reads: AtomicUsize,
    lineage_reads: AtomicUsize,
    reads: AtomicUsize,
    writes: AtomicUsize,
    forbidden: Mutex<BTreeSet<ObjectId>>,
    forbidden_reads: AtomicUsize,
}

impl CountingStore {
    fn new(inner: LooseObjectStore) -> Self {
        Self {
            inner,
            tree_reads: AtomicUsize::new(0),
            lineage_reads: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            forbidden: Mutex::new(BTreeSet::new()),
            forbidden_reads: AtomicUsize::new(0),
        }
    }

    fn reset(&self, forbidden: BTreeSet<ObjectId>) {
        self.tree_reads.store(0, Ordering::Relaxed);
        self.lineage_reads.store(0, Ordering::Relaxed);
        self.reads.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
        self.forbidden_reads.store(0, Ordering::Relaxed);
        *self.forbidden.lock().unwrap() = forbidden;
    }
}

#[async_trait]
impl ImmutableObjectStore for CountingStore {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.forbidden.lock().unwrap().contains(&id) {
            self.forbidden_reads.fetch_add(1, Ordering::Relaxed);
        }
        let object = self.inner.get(id).await?;
        match object.kind {
            ObjectKind::Tree => {
                self.tree_reads.fetch_add(1, Ordering::Relaxed);
            }
            ObjectKind::ViewLineageMapNode => {
                self.lineage_reads.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        Ok(object)
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

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

fn revision(path: &str, expected: Option<&[u8]>, replacement: &[u8], old: EditId) -> RevisionEdits {
    let mut edits = exact(path, expected, replacement);
    edits.transition.removed.insert(old);
    edits
}

fn proposal(
    id: ChangeId,
    dependencies: impl IntoIterator<Item = ChangeId>,
    edits: RevisionEdits,
) -> ChangeProposal {
    ChangeProposal {
        id,
        metadata: ChangeMetadata::default(),
        dependencies: dependencies.into_iter().collect(),
        revision_metadata: RevisionMetadata::default(),
        edits,
    }
}

#[tokio::test]
async fn root_backed_create_and_revise_match_reference_and_preserve_roots_and_labels() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in [b"a1".as_slice(), b"a2", b"b1", b"c1", b"resolved"] {
        content.put(bytes).unwrap();
    }

    let mut reference = Repository::new(Tree::new());
    let base = reference.base_view().id;
    let mut effective = base;
    let a = ChangeId::new();
    let a1_edits = exact("a", None, b"a1");
    let a1_edit = *a1_edits.edits.keys().next().unwrap();
    let a1 = reference
        .create_change_with_id(
            base,
            effective,
            a,
            [],
            a1_edits,
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = a1.resulting_view;
    let b = ChangeId::new();
    let b1 = reference
        .create_change_with_id(
            base,
            effective,
            b,
            [],
            exact("b", None, b"b1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = b1.resulting_view;

    let labels = ViewLabelManifestRecord::new([("main", effective), ("archive", base)]).unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial_closure = publisher.prepare(&reference).await.unwrap();
    let initial_envelope = publisher
        .prepare_envelope(initial_closure.state.id, &labels)
        .await
        .unwrap();
    let initial = publisher
        .initialize_prepared_envelope(initial_envelope)
        .await
        .unwrap();

    let c = ChangeId::new();
    let c_proposal = proposal(c, [a, b], exact("a", Some(b"a1"), b"c1"));
    let mutation_effective = effective;
    let expected_create = reference
        .create_change_with_metadata(
            base,
            effective,
            c,
            ChangeMetadata::default(),
            [a, b],
            RevisionMetadata::default(),
            exact("a", Some(b"a1"), b"c1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = expected_create.resulting_view;
    let expected_create_state = reference.canonical_closure().state.id;

    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, actual_create) = mutator
        .create(initial, base, mutation_effective, c_proposal)
        .await
        .unwrap();
    assert_eq!(actual_create, expected_create);
    assert_eq!(prepared.state(), expected_create_state);
    // Preparing immutable objects does not publish anything.
    assert_eq!(heads.read().unwrap(), initial);
    let after_create = mutator.publish(prepared).await.unwrap();

    let reader = RepositoryReader::new(&objects, &content);
    let create_envelope = reader.envelope(after_create.envelope).await.unwrap();
    assert_eq!(create_envelope.labels, labels.id());
    let create_state = reader.state(create_envelope.state).await.unwrap();
    let retained = RetainedSetStore::new(&objects);
    assert!(
        retained
            .contains(create_state.retained_views, effective.into())
            .await
            .unwrap()
    );
    assert!(
        retained
            .contains(create_state.retained_revisions, a1.revision_id.into())
            .await
            .unwrap()
    );

    let a2_edits = revision("a", None, b"a2", a1_edit);
    let mutation_effective = effective;
    let expected_revise = reference
        .revise_change(
            base,
            effective,
            a,
            a1.revision_id,
            a2_edits.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = expected_revise.resulting_view;
    assert_eq!(expected_revise.reapplied, vec![c]);
    assert!(
        !reference
            .view(effective)
            .unwrap()
            .unresolved_conflicts
            .is_empty()
    );
    let expected_revise_state = reference.canonical_closure().state.id;

    let (prepared, actual_revise) = mutator
        .revise(
            after_create,
            base,
            mutation_effective,
            a,
            a1.revision_id,
            RevisionMetadata::default(),
            a2_edits,
        )
        .await
        .unwrap();
    assert_eq!(actual_revise, expected_revise);
    assert_eq!(prepared.state(), expected_revise_state);
    let after_revise = mutator.publish(prepared).await.unwrap();
    let envelope = reader.envelope(after_revise.envelope).await.unwrap();
    assert_eq!(envelope.labels, labels.id());
    reader.validate_complete(envelope.state).await.unwrap();
    assert_eq!(
        reader
            .hydrate(envelope.state)
            .await
            .unwrap()
            .canonical_closure()
            .state
            .id,
        expected_revise_state
    );

    let conflict_revision = reference.current_revision(effective, c).unwrap();
    let conflicted = reference
        .conflicted_application(effective, conflict_revision)
        .unwrap()
        .clone();
    let resolved_value = FileValue {
        content: ContentId::of(b"resolved"),
        kind: FileKind::Regular,
    };
    let mut resolved_tree = conflicted.result.materialized_tree().clone();
    resolved_tree.insert("a".into(), resolved_value);
    let clean_application = reference.current_application(effective, a).unwrap().id;
    let stale = mutator
        .resolve_conflict(
            after_revise,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision: a1.revision_id,
                application: conflicted.id,
                resolved: BTreeMap::new(),
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(
        stale,
        RepositoryMutationError::Model(RepositoryError::StaleRevision(id)) if id == a
    ));
    let not_conflicted = mutator
        .resolve_conflict(
            after_revise,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision: expected_revise.revision_id,
                application: clean_application,
                resolved: BTreeMap::new(),
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(
        not_conflicted,
        RepositoryMutationError::Model(RepositoryError::NotConflicted(id))
            if id == expected_revise.revision_id
    ));
    let outside = mutator
        .resolve_conflict(
            after_revise,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision,
                application: conflicted.id,
                resolved: [("b".into(), Some(resolved_value))].into(),
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(
        outside,
        RepositoryMutationError::Model(RepositoryError::InvalidConflictResolution)
    ));
    let missing_content = mutator
        .resolve_conflict(
            after_revise,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision,
                application: conflicted.id,
                resolved: [(
                    "a".into(),
                    Some(FileValue {
                        content: ContentId::of(b"missing resolution content"),
                        kind: FileKind::Regular,
                    }),
                )]
                .into(),
            },
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(
        missing_content,
        RepositoryMutationError::Model(RepositoryError::ContentIo(_))
    ));
    assert_eq!(heads.read().unwrap(), after_revise);

    let expected_resolution = reference
        .resolve_conflict(
            base,
            effective,
            ConflictResolution {
                conflict_revision,
                resolved_tree,
            },
            ApplicationContext::new(&content),
        )
        .unwrap();
    let expected_resolution_state = reference.canonical_closure().state.id;

    let (prepared, actual_resolution) = mutator
        .resolve_conflict(
            after_revise,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision,
                application: conflicted.id,
                resolved: [("a".into(), Some(resolved_value))].into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(actual_resolution, expected_resolution);
    assert_eq!(prepared.state(), expected_resolution_state);
    let after_resolution = mutator.publish(prepared).await.unwrap();
    let envelope = reader.envelope(after_resolution.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
    let hydrated = reader.hydrate(envelope.state).await.unwrap();
    assert_eq!(
        hydrated.canonical_closure().state.id,
        expected_resolution_state
    );
    let revision = hydrated.revision(actual_resolution.revision_id).unwrap();
    assert_eq!(revision.edit_transition().resolutions.len(), 1);
    assert_eq!(
        revision.edit_transition().resolutions[0].application,
        conflicted.id
    );
}

#[tokio::test]
async fn mutation_returns_and_retains_result_without_moving_labels() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    content.put(b"value").unwrap();
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let labels = ViewLabelManifestRecord::new([("main", base)]).unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let closure = publisher.prepare(&repository).await.unwrap();
    let envelope = publisher
        .prepare_envelope(closure.state.id, &labels)
        .await
        .unwrap();
    let initial = publisher
        .initialize_prepared_envelope(envelope)
        .await
        .unwrap();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, outcome) = mutator
        .create(
            initial,
            base,
            base,
            proposal(ChangeId::new(), [], exact("path", None, b"value")),
        )
        .await
        .unwrap();
    assert_eq!(prepared.resulting_view(), outcome.resulting_view);
    let resulting = prepared.resulting_view();
    let head = mutator.publish(prepared).await.unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    assert_eq!(envelope.labels, labels.id());
    let state = reader.state(envelope.state).await.unwrap();
    assert!(
        RetainedSetStore::new(&objects)
            .contains(state.retained_views, resulting.into())
            .await
            .unwrap()
    );
    assert!(
        ViewLineageMapStore::new(&objects)
            .is_descendant(
                state.view_lineage,
                base,
                resulting,
                ViewLineageLimits::default(),
            )
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_content_resolution_never_prepares_or_advances_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let effective = base;
    let initial = publisher.initialize(&repository).await.unwrap();
    let before = reader_state(&objects, &content, initial).await;

    let result = PersistedRepositoryMutator::new(&objects, &content, &heads)
        .create(
            initial,
            base,
            effective,
            proposal(ChangeId::new(), [], exact("missing", None, b"not retained")),
        )
        .await;
    assert!(result.is_err());
    assert_eq!(heads.read().unwrap(), initial);
    assert_eq!(reader_state(&objects, &content, initial).await, before);
}

#[tokio::test]
async fn conflict_resolution_deletion_mode_and_symlink_match_hydrated_reference() {
    for resolved in [
        None,
        Some((b"authored".as_slice(), FileKind::Executable)),
        Some((b"destination".as_slice(), FileKind::Symlink)),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        for bytes in [b"base".as_slice(), b"authored", b"upstream", b"destination"] {
            content.put(bytes).unwrap();
        }

        let mut reference = Repository::new(Tree::new());
        let base = reference.base_view().id;
        let mut effective = base;
        let parent = ChangeId::new();
        let parent_edits = exact("file", None, b"base");
        let parent_edit = *parent_edits.edits.keys().next().unwrap();
        let parent_outcome = reference
            .create_change_with_id(
                base,
                effective,
                parent,
                [],
                parent_edits,
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = parent_outcome.resulting_view;
        let child = ChangeId::new();
        let child_outcome = reference
            .create_change_with_id(
                base,
                effective,
                child,
                [parent],
                exact("file", Some(b"base"), b"authored"),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = child_outcome.resulting_view;
        let publisher = RepositoryPublisher::new(&objects, &content, &heads);
        let head = publisher.initialize(&reference).await.unwrap();

        let parent_revision = revision("file", None, b"upstream", parent_edit);
        let mutation_effective = effective;
        let expected_revise = reference
            .revise_change(
                base,
                effective,
                parent,
                parent_outcome.revision_id,
                parent_revision.clone(),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = expected_revise.resulting_view;
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (prepared, actual_revise) = mutator
            .revise(
                head,
                base,
                mutation_effective,
                parent,
                parent_outcome.revision_id,
                RevisionMetadata::default(),
                parent_revision,
            )
            .await
            .unwrap();
        assert_eq!(actual_revise, expected_revise);
        let conflicted_head = mutator.publish(prepared).await.unwrap();

        let conflict_revision = reference.current_revision(effective, child).unwrap();
        let application = reference
            .conflicted_application(effective, conflict_revision)
            .unwrap()
            .clone();
        let value = resolved.map(|(bytes, kind)| FileValue {
            content: ContentId::of(bytes),
            kind,
        });
        let mut resolved_tree = application.result.materialized_tree().clone();
        if let Some(value) = value {
            resolved_tree.insert("file".into(), value);
        } else {
            resolved_tree.remove("file");
        }
        let expected = reference
            .resolve_conflict(
                base,
                effective,
                ConflictResolution {
                    conflict_revision,
                    resolved_tree,
                },
                ApplicationContext::new(&content),
            )
            .unwrap();
        let expected_state = reference.canonical_closure().state.id;
        let (prepared, actual) = mutator
            .resolve_conflict(
                conflicted_head,
                base,
                effective,
                PersistedConflictResolution {
                    conflict_revision,
                    application: application.id,
                    resolved: [("file".into(), value)].into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(prepared.state(), expected_state);
        let resolved_head = mutator.publish(prepared).await.unwrap();
        let reader = RepositoryReader::new(&objects, &content);
        let envelope = reader.envelope(resolved_head.envelope).await.unwrap();
        reader.validate_complete(envelope.state).await.unwrap();
        assert_eq!(
            reader
                .hydrate(envelope.state)
                .await
                .unwrap()
                .canonical_closure()
                .state
                .id,
            expected_state
        );
    }
}

async fn reader_state(
    objects: &LooseObjectStore,
    content: &ContentStore,
    head: RepositoryHead,
) -> RepositoryStateId {
    RepositoryReader::new(objects, content)
        .envelope(head.envelope)
        .await
        .unwrap()
        .state
}

#[tokio::test]
async fn stale_prepare_and_stale_publish_never_advance_the_head() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let effective = base;
    let initial = publisher.initialize(&repository).await.unwrap();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);

    let (first, _) = mutator
        .create(
            initial,
            base,
            effective,
            proposal(ChangeId::new(), BTreeSet::new(), RevisionEdits::default()),
        )
        .await
        .unwrap();
    let (loser, _) = mutator
        .create(
            initial,
            base,
            effective,
            proposal(ChangeId::new(), BTreeSet::new(), RevisionEdits::default()),
        )
        .await
        .unwrap();
    let winner = mutator.publish(first).await.unwrap();
    assert!(matches!(
        mutator.publish(loser).await,
        Err(RepositoryMutationError::Stale { .. })
    ));
    assert!(matches!(
        mutator
            .create(
                initial,
                base,
                effective,
                proposal(ChangeId::new(), [], RevisionEdits::default())
            )
            .await,
        Err(RepositoryMutationError::Stale { .. })
    ));
    assert_eq!(heads.read().unwrap(), winner);
}

#[tokio::test]
async fn mutation_reads_only_touched_tree_paths_and_current_records() {
    let dir = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(dir.path().join("objects")));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));

    let mut tree = Tree::new();
    for index in 0..2_000 {
        let bytes = format!("value-{index}").into_bytes();
        let id = content.put(&bytes).unwrap();
        tree.insert(
            format!("directory-{index}/file"),
            FileValue {
                content: id,
                kind: FileKind::Regular,
            },
        );
    }
    for bytes in [b"one".as_slice(), b"two", b"changed"] {
        content.put(bytes).unwrap();
    }
    let mut reference = Repository::new(tree);
    let base = reference.base_view().id;
    let mut effective = base;
    let change = ChangeId::new();
    let first = reference
        .create_change_with_id(
            base,
            effective,
            change,
            [],
            exact("touched", None, b"one"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = first.resulting_view;
    let first_view = effective;
    let first_application = reference.current_application(effective, change).unwrap().id;
    let first_edit = *reference
        .revision(first.revision_id)
        .unwrap()
        .edits()
        .keys()
        .next()
        .unwrap();
    let second = reference
        .revise_change(
            base,
            effective,
            change,
            first.revision_id,
            revision("touched", None, b"two", first_edit),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = second.resulting_view;
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let head = publisher.initialize(&reference).await.unwrap();
    objects.reset(BTreeSet::from([
        first.revision_id.into(),
        first_view.into(),
        first_application.into(),
    ]));

    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let second_edit = *reference
        .revision(second.revision_id)
        .unwrap()
        .edits()
        .keys()
        .next()
        .unwrap();
    mutator
        .revise(
            head,
            base,
            effective,
            change,
            second.revision_id,
            RevisionMetadata::default(),
            revision("touched", None, b"changed", second_edit),
        )
        .await
        .unwrap();

    assert_eq!(objects.forbidden_reads.load(Ordering::Relaxed), 0);
    assert!(objects.tree_reads.load(Ordering::Relaxed) < 20);
}

#[tokio::test]
async fn frontier_create_and_tail_revision_reuse_a_long_persisted_prefix() {
    const CHAIN: usize = 96;
    let dir = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(dir.path().join("objects")));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let mut reference = Repository::new(Tree::new());
    let base = reference.base_view().id;
    let mut effective = base;
    let mut previous = None;
    let mut old_objects = BTreeSet::new();

    for index in 0..CHAIN {
        let change = ChangeId::from_bytes((index as u128 + 1).to_be_bytes());
        let bytes = format!("value-{index}").into_bytes();
        content.put(&bytes).unwrap();
        let edits = exact(&format!("file-{index}"), None, &bytes);
        old_objects.extend(edits.edits.keys().copied().map(ObjectId::from));
        let outcome = reference
            .create_change_with_id(
                base,
                effective,
                change,
                previous,
                edits,
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
        previous = Some(change);
    }
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let head = publisher.initialize(&reference).await.unwrap();
    let tail = ChangeId::from_bytes((CHAIN as u128 + 1).to_be_bytes());
    content.put(b"tail-one").unwrap();
    objects.reset(old_objects.clone());

    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let tail_edits = exact("tail", None, b"tail-one");
    let tail_edit = *tail_edits.edits.keys().next().unwrap();
    let mutation_effective = effective;
    let expected = reference
        .create_change_with_id(
            base,
            effective,
            tail,
            previous,
            tail_edits.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    effective = expected.resulting_view;
    let expected_state = reference.canonical_closure().state.id;
    // Frontier creation derives the chain tip after opening the persisted state. The canonical
    // reference proposal names that tip explicitly, proving both dependency and identity parity.
    let (prepared, actual) = mutator
        .create_on_frontier(
            head,
            base,
            mutation_effective,
            proposal(tail, [], tail_edits),
        )
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(prepared.state(), expected_state);
    assert_eq!(objects.forbidden_reads.load(Ordering::Relaxed), 0);
    assert!(objects.tree_reads.load(Ordering::Relaxed) < 20);
    assert!(objects.writes.load(Ordering::Relaxed) < 100);
    let head = mutator.publish(prepared).await.unwrap();

    // Revising the canonical tail anchors at the view immediately before it and reapplies only it.
    // Its own predecessor edit must be read to validate lineage; the historical prefix edits must not.
    content.put(b"tail-two").unwrap();
    objects.reset(old_objects);
    let revised = revision("tail", None, b"tail-two", tail_edit);
    let predecessor = expected.revision_id;
    let expected = reference
        .revise_change(
            base,
            effective,
            tail,
            predecessor,
            revised.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let expected_state = reference.canonical_closure().state.id;
    let (prepared, actual) = mutator
        .revise(
            head,
            base,
            effective,
            tail,
            predecessor,
            RevisionMetadata::default(),
            revised,
        )
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(prepared.state(), expected_state);
    assert_eq!(objects.forbidden_reads.load(Ordering::Relaxed), 0);
    assert!(objects.tree_reads.load(Ordering::Relaxed) < 20);
    assert!(objects.writes.load(Ordering::Relaxed) < 100);
}

/// Where the next mutation goes, and the top change's latest revision and edit.
struct Stack {
    base: ViewId,
    effective: ViewId,
    head: RepositoryHead,
    top: ChangeId,
    revision: RevisionId,
    edit: EditId,
}

/// Publishes `depth` changes one mutation at a time, each created on top and revised
/// until it has `per_change` revisions, the way repeated `wheat add` calls build a stack
/// change by change.
async fn spread_history(
    objects: &CountingStore,
    content: &ContentStore,
    heads: &LocalPublicationStore,
    depth: usize,
    per_change: usize,
) -> Stack {
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut head = RepositoryPublisher::new(objects, content, heads)
        .initialize(&repository)
        .await
        .unwrap();
    let mutator = PersistedRepositoryMutator::new(objects, content, heads);
    let mut effective = base;
    let mut below = None;
    let mut stack = None;
    for index in 0..depth {
        let change = ChangeId::new();
        let path = format!("file-{index}");
        let bytes = format!("{index}-0").into_bytes();
        content.put(&bytes).unwrap();
        let edits = exact(&path, None, &bytes);
        let mut edit = *edits.edits.keys().next().unwrap();
        let (prepared, outcome) = mutator
            .create(head, base, effective, proposal(change, below, edits))
            .await
            .unwrap();
        head = mutator.publish(prepared).await.unwrap();
        effective = outcome.resulting_view;
        let mut revision_id = outcome.revision_id;
        for checkpoint in 1..per_change {
            let bytes = format!("{index}-{checkpoint}").into_bytes();
            content.put(&bytes).unwrap();
            let edits = revision(&path, None, &bytes, edit);
            let next_edit = *edits.edits.keys().next().unwrap();
            let (prepared, outcome) = mutator
                .revise(
                    head,
                    base,
                    effective,
                    change,
                    revision_id,
                    RevisionMetadata::default(),
                    edits,
                )
                .await
                .unwrap();
            head = mutator.publish(prepared).await.unwrap();
            effective = outcome.resulting_view;
            revision_id = outcome.revision_id;
            edit = next_edit;
        }
        below = Some(change);
        stack = Some(Stack {
            base,
            effective,
            head,
            top: change,
            revision: revision_id,
            edit,
        });
    }
    stack.expect("depth is at least one")
}

/// Lineage-map reads for one more revision of the top change of a spread history.
async fn lineage_reads_for_top_revision(depth: usize, per_change: usize) -> usize {
    // Hundreds of publications: defer the syncs, which only cost time here.
    let dir = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::with_durability(
        dir.path().join("objects"),
        Durability::Deferred,
    ));
    let content = ContentStore::with_durability(dir.path().join("content"), Durability::Deferred);
    let heads =
        LocalPublicationStore::with_durability(dir.path().join("head"), Durability::Deferred);
    let stack = spread_history(&objects, &content, &heads, depth, per_change).await;
    content.put(b"final").unwrap();
    objects.reset(BTreeSet::new());
    let (_prepared, _) = PersistedRepositoryMutator::new(&objects, &content, &heads)
        .revise(
            stack.head,
            stack.base,
            stack.effective,
            stack.top,
            stack.revision,
            RevisionMetadata::default(),
            revision(&format!("file-{}", depth - 1), None, b"final", stack.edit),
        )
        .await
        .unwrap();
    objects.lineage_reads.load(Ordering::Relaxed)
}

#[tokio::test]
async fn proving_ancestry_does_not_grow_with_the_history_under_the_basis() {
    // Same stack depth; the long history has 20 times as many revisions behind the top.
    let short = lineage_reads_for_top_revision(6, 2).await;
    let long = lineage_reads_for_top_revision(6, 40).await;
    assert!(
        long <= short + 12,
        "a revision read {long} lineage-map nodes after a long history, {short} after a short one"
    );
}

#[tokio::test]
async fn a_mutation_refuses_an_effective_view_outside_the_basis_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(dir.path().join("objects")));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let stack = spread_history(&objects, &content, &heads, 2, 3).await;
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);

    // A sibling branch straight off the base: retained, but not below the first stack.
    content.put(b"sibling").unwrap();
    let (prepared, sibling) = mutator
        .create(
            stack.head,
            stack.base,
            stack.base,
            proposal(ChangeId::new(), [], exact("sibling", None, b"sibling")),
        )
        .await
        .unwrap();
    let head = mutator.publish(prepared).await.unwrap();

    content.put(b"refused").unwrap();
    for (basis, effective) in [
        // The base is an ancestor of the stack, not a descendant.
        (stack.effective, stack.base),
        // Neither branch descends from the other.
        (stack.effective, sibling.resulting_view),
        (sibling.resulting_view, stack.effective),
    ] {
        let refused = mutator
            .create(
                head,
                basis,
                effective,
                proposal(ChangeId::new(), [], exact("refused", None, b"refused")),
            )
            .await
            .err();
        assert!(
            matches!(
                refused,
                Some(RepositoryMutationError::Publication(
                    RepositoryPublicationError::InvalidReferences(
                        "effective view does not descend from basis view"
                    )
                ))
            ),
            "basis {basis} and effective {effective}: {refused:?}"
        );
    }
    assert_eq!(heads.read().unwrap(), head);
}

/// A published chain of changes, each depending on the one before.
struct PublishedChain {
    _dir: tempfile::TempDir,
    objects: CountingStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    reference: Repository,
    base: ViewId,
    effective: ViewId,
    head: RepositoryHead,
    /// Each change in canonical order, with its revision and its one edit.
    changes: Vec<(ChangeId, RevisionId, EditId)>,
}

/// The ID of the chain's change at `position`: `2, 4, 6, ...`, so that a change can be
/// given an ID between any two.
fn chain_id(position: usize) -> ChangeId {
    ChangeId::from_bytes((2 * (position as u128 + 1)).to_be_bytes())
}

async fn published_chain(length: usize) -> PublishedChain {
    let dir = tempfile::tempdir().unwrap();
    let objects = CountingStore::new(LooseObjectStore::new(dir.path().join("objects")));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let mut reference = Repository::new(Tree::new());
    let base = reference.base_view().id;
    let mut effective = base;
    let mut changes = Vec::with_capacity(length);
    let mut previous = None;
    for position in 0..length {
        let change = chain_id(position);
        let bytes = format!("value-{position}").into_bytes();
        content.put(&bytes).unwrap();
        let edits = exact(&format!("file-{position}"), None, &bytes);
        let edit = *edits.edits.keys().next().unwrap();
        let outcome = reference
            .create_change_with_id(
                base,
                effective,
                change,
                previous,
                edits,
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
        changes.push((change, outcome.revision_id, edit));
        previous = Some(change);
    }
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&reference)
        .await
        .unwrap();
    PublishedChain {
        _dir: dir,
        objects,
        content,
        heads,
        reference,
        base,
        effective,
        head,
        changes,
    }
}

impl PublishedChain {
    fn mutator(&self) -> PersistedRepositoryMutator<'_, CountingStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    /// Resets the counts, and counts reads of the revisions of the changes below
    /// `position` as forbidden.
    fn forbid_revisions_below(&self, position: usize) {
        self.objects.reset(
            self.changes[..position]
                .iter()
                .map(|(_, revision, _)| (*revision).into())
                .collect(),
        );
    }

    fn forbidden_reads(&self) -> usize {
        self.objects.forbidden_reads.load(Ordering::Relaxed)
    }
}

/// The history lengths the read-count test compares.
const HISTORIES: [usize; 2] = [32, 256];

/// Creating a change on the frontier reads none of the revisions below it, and its other
/// reads (radix paths, the order index's skip chain) grow by far less than one per change
/// of history: reading the history would cost at least one per change.
#[tokio::test]
async fn creating_on_top_reads_no_revision_below_it() {
    let mut reads = Vec::new();
    for length in HISTORIES {
        // Hundreds of publications: defer the syncs, which only cost time here.
        let dir = tempfile::tempdir().unwrap();
        let objects = CountingStore::new(LooseObjectStore::with_durability(
            dir.path().join("objects"),
            Durability::Deferred,
        ));
        let content =
            ContentStore::with_durability(dir.path().join("content"), Durability::Deferred);
        let heads =
            LocalPublicationStore::with_durability(dir.path().join("head"), Durability::Deferred);
        let stack = spread_history(&objects, &content, &heads, length, 1).await;
        let reader = RepositoryReader::new(&objects, &content);
        let mut history = BTreeSet::new();
        let mut revisions = RevisionMapStore::new(&objects)
            .cursor(reader.view(stack.effective).await.unwrap().revisions)
            .await
            .unwrap();
        while let Some((_, revision)) = revisions.next().await.unwrap() {
            history.insert(revision.into());
        }
        assert_eq!(history.len(), length);
        content.put(b"top").unwrap();
        objects.reset(history);
        let (_, outcome) = PersistedRepositoryMutator::new(&objects, &content, &heads)
            .create_on_frontier(
                stack.head,
                stack.base,
                stack.effective,
                proposal(ChangeId::new(), [], exact("top", None, b"top")),
            )
            .await
            .unwrap();
        assert_eq!(objects.forbidden_reads.load(Ordering::Relaxed), 0);
        reads.push(objects.reads.load(Ordering::Relaxed));
        let created = reader.revision(outcome.revision_id).await.unwrap();
        assert_eq!(created.dependencies, BTreeSet::from([stack.top]));
    }
    assert!(
        reads[1] < reads[0] + (HISTORIES[1] - HISTORIES[0]) / 4,
        "reads grew with the history: {reads:?}"
    );
}

/// Revising a change reads the revisions from it up, none below.
#[tokio::test]
async fn revising_a_change_reads_no_revision_below_it() {
    let mut chain = published_chain(64).await;
    let (change, predecessor, edit) = chain.changes[48];
    chain.content.put(b"revised").unwrap();
    let edits = revision("file-48", None, b"revised", edit);
    let expected = chain
        .reference
        .revise_change(
            chain.base,
            chain.effective,
            change,
            predecessor,
            edits.clone(),
            ApplicationContext::new(&chain.content),
        )
        .unwrap();
    chain.forbid_revisions_below(48);
    let (prepared, actual) = chain
        .mutator()
        .revise(
            chain.head,
            chain.base,
            chain.effective,
            change,
            predecessor,
            RevisionMetadata::default(),
            edits,
        )
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        prepared.state(),
        chain.reference.canonical_closure().state.id
    );
    assert_eq!(chain.forbidden_reads(), 0);
}

/// A change whose ID sorts below later changes goes between them, at the first change
/// above its dependency with a higher ID. The changes from its dependency up are read and
/// those above its spot composed again; none below its dependency is read.
#[tokio::test]
async fn a_change_sorting_below_later_changes_reads_from_its_dependency_up() {
    let mut chain = published_chain(32).await;
    // Between the changes at positions 19 and 20, depending on the one at 10.
    let inserted = ChangeId::from_bytes(41u128.to_be_bytes());
    let dependency = chain.changes[10].0;
    chain.content.put(b"inserted").unwrap();
    let edits = exact("inserted", None, b"inserted");
    let expected = chain
        .reference
        .create_change_with_id(
            chain.base,
            chain.effective,
            inserted,
            [dependency],
            edits.clone(),
            ApplicationContext::new(&chain.content),
        )
        .unwrap();
    chain.forbid_revisions_below(11);
    let (prepared, actual) = chain
        .mutator()
        .create(
            chain.head,
            chain.base,
            chain.effective,
            proposal(inserted, [dependency], edits),
        )
        .await
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(
        prepared.state(),
        chain.reference.canonical_closure().state.id
    );
    assert_eq!(chain.forbidden_reads(), 0);
}
