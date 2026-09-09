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
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            forbidden: Mutex::new(BTreeSet::new()),
            forbidden_reads: AtomicUsize::new(0),
        }
    }

    fn reset(&self, forbidden: BTreeSet<ObjectId>) {
        self.tree_reads.store(0, Ordering::Relaxed);
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
        if object.kind == ObjectKind::Tree {
            self.tree_reads.fetch_add(1, Ordering::Relaxed);
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
async fn root_backed_create_and_revise_match_reference_and_preserve_roots_and_branches() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in [b"a1".as_slice(), b"a2", b"b1", b"c1", b"resolved"] {
        content.put(bytes).unwrap();
    }

    let mut reference = Repository::new(Tree::new());
    let a = ChangeId::new();
    let a1_edits = exact("a", None, b"a1");
    let a1_edit = *a1_edits.edits.keys().next().unwrap();
    let a1 = reference
        .create_change_with_id(a, [], a1_edits, ApplicationContext::new(&content))
        .unwrap();
    let b = ChangeId::new();
    reference
        .create_change_with_id(
            b,
            [],
            exact("b", None, b"b1"),
            ApplicationContext::new(&content),
        )
        .unwrap();

    let branches = BranchManifestRecord::new([
        ("refs/heads/main", reference.working_view().id),
        ("refs/heads/archive", reference.published_view().id),
    ])
    .unwrap();
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial_closure = publisher.prepare(&reference).await.unwrap();
    let initial_envelope = publisher
        .prepare_envelope(initial_closure.state.id, &branches)
        .await
        .unwrap();
    let initial = publisher
        .initialize_prepared_envelope(initial_envelope)
        .await
        .unwrap();

    let c = ChangeId::new();
    let c_proposal = proposal(c, [a, b], exact("a", Some(b"a1"), b"c1"));
    let expected_create = reference
        .create_change_with_metadata(
            c,
            ChangeMetadata::default(),
            [a, b],
            RevisionMetadata::default(),
            exact("a", Some(b"a1"), b"c1"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let expected_create_state = reference.canonical_closure().state.id;

    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, actual_create) = mutator.create(initial, c_proposal).await.unwrap();
    assert_eq!(actual_create, expected_create);
    assert_eq!(prepared.state(), expected_create_state);
    // Preparing immutable objects does not publish anything.
    assert_eq!(heads.read().unwrap(), initial);
    let after_create = mutator.publish(prepared).await.unwrap();

    let reader = RepositoryReader::new(&objects, &content);
    let create_envelope = reader.envelope(after_create.envelope).await.unwrap();
    assert_eq!(create_envelope.branches, branches.id());
    let create_state = reader.state(create_envelope.state).await.unwrap();
    let retained = RetainedSetStore::new(&objects);
    assert!(
        retained
            .contains(
                create_state.retained_views,
                initial_closure.state.working_view.into()
            )
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
    let expected_revise = reference
        .revise_change(
            a,
            a1.revision_id,
            a2_edits.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    assert_eq!(expected_revise.reapplied, vec![c]);
    assert!(!reference.working_view().unresolved_conflicts.is_empty());
    let expected_revise_state = reference.canonical_closure().state.id;

    let (prepared, actual_revise) = mutator
        .revise(
            after_create,
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
    assert_eq!(envelope.branches, branches.id());
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

    let conflict_revision = reference.current_revision(c).unwrap();
    let conflicted = reference
        .conflicted_application(conflict_revision)
        .unwrap()
        .clone();
    let resolved_value = FileValue {
        content: ContentId::of(b"resolved"),
        kind: FileKind::Regular,
    };
    let mut resolved_tree = conflicted.result.materialized_tree().clone();
    resolved_tree.insert("a".into(), resolved_value);
    let clean_application = reference.current_application(a).unwrap().id;
    let stale = mutator
        .resolve_conflict(
            after_revise,
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
async fn publication_target_defaults_to_working_only_and_can_publish_working() {
    for target in [MutationTarget::WorkingOnly, MutationTarget::PublishWorking] {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        content.put(b"value").unwrap();
        let initial = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&Repository::new(Tree::new()))
            .await
            .unwrap();
        let mutator =
            PersistedRepositoryMutator::new(&objects, &content, &heads).with_target(target);
        let (prepared, _) = mutator
            .create(
                initial,
                proposal(ChangeId::new(), [], exact("path", None, b"value")),
            )
            .await
            .unwrap();
        let head = mutator.publish(prepared).await.unwrap();
        let reader = RepositoryReader::new(&objects, &content);
        let state = reader
            .state(reader_state(&objects, &content, head).await)
            .await
            .unwrap();
        let initial_state = reader
            .state(reader_state(&objects, &content, initial).await)
            .await
            .unwrap();
        assert_ne!(state.working_view, initial_state.working_view);
        match target {
            MutationTarget::WorkingOnly => assert_ne!(state.working_view, state.published_view),
            MutationTarget::PublishWorking => assert_eq!(state.working_view, state.published_view),
        }
    }
}

#[tokio::test]
async fn failed_content_resolution_never_prepares_or_advances_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let publisher = RepositoryPublisher::new(&objects, &content, &heads);
    let initial = publisher
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let before = reader_state(&objects, &content, initial).await;

    let result = PersistedRepositoryMutator::new(&objects, &content, &heads)
        .create(
            initial,
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
        let parent = ChangeId::new();
        let parent_edits = exact("file", None, b"base");
        let parent_edit = *parent_edits.edits.keys().next().unwrap();
        let parent_outcome = reference
            .create_change_with_id(parent, [], parent_edits, ApplicationContext::new(&content))
            .unwrap();
        let child = ChangeId::new();
        reference
            .create_change_with_id(
                child,
                [parent],
                exact("file", Some(b"base"), b"authored"),
                ApplicationContext::new(&content),
            )
            .unwrap();
        let publisher = RepositoryPublisher::new(&objects, &content, &heads);
        let head = publisher.initialize(&reference).await.unwrap();

        let parent_revision = revision("file", None, b"upstream", parent_edit);
        let expected_revise = reference
            .revise_change(
                parent,
                parent_outcome.revision_id,
                parent_revision.clone(),
                ApplicationContext::new(&content),
            )
            .unwrap();
        let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
        let (prepared, actual_revise) = mutator
            .revise(
                head,
                parent,
                parent_outcome.revision_id,
                RevisionMetadata::default(),
                parent_revision,
            )
            .await
            .unwrap();
        assert_eq!(actual_revise, expected_revise);
        let conflicted_head = mutator.publish(prepared).await.unwrap();

        let conflict_revision = reference.current_revision(child).unwrap();
        let application = reference
            .conflicted_application(conflict_revision)
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
    let initial = publisher
        .initialize(&Repository::new(Tree::new()))
        .await
        .unwrap();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);

    let (first, _) = mutator
        .create(
            initial,
            proposal(ChangeId::new(), BTreeSet::new(), RevisionEdits::default()),
        )
        .await
        .unwrap();
    let (loser, _) = mutator
        .create(
            initial,
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
    let change = ChangeId::new();
    let first = reference
        .create_change_with_id(
            change,
            [],
            exact("touched", None, b"one"),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let first_view = reference.working_view().id;
    let first_application = reference.current_application(change).unwrap().id;
    let first_edit = *reference
        .revision(first.revision_id)
        .unwrap()
        .edits()
        .keys()
        .next()
        .unwrap();
    let second = reference
        .revise_change(
            change,
            first.revision_id,
            revision("touched", None, b"two", first_edit),
            ApplicationContext::new(&content),
        )
        .unwrap();
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
    let mut previous = None;
    let mut old_objects = BTreeSet::new();

    for index in 0..CHAIN {
        let change = ChangeId::from_bytes((index as u128 + 1).to_be_bytes());
        let bytes = format!("value-{index}").into_bytes();
        content.put(&bytes).unwrap();
        let edits = exact(&format!("file-{index}"), None, &bytes);
        old_objects.extend(edits.edits.keys().copied().map(ObjectId::from));
        reference
            .create_change_with_id(change, previous, edits, ApplicationContext::new(&content))
            .unwrap();
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
    let expected = reference
        .create_change_with_id(
            tail,
            previous,
            tail_edits.clone(),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let expected_state = reference.canonical_closure().state.id;
    // Frontier creation derives the chain tip after opening the persisted state. The canonical
    // reference proposal names that tip explicitly, proving both dependency and identity parity.
    let (prepared, actual) = mutator
        .create_on_frontier(head, proposal(tail, [], tail_edits))
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
