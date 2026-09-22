use std::collections::{BTreeMap, BTreeSet};

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
            ..Default::default()
        },
    }
}

async fn fixture() -> (
    tempfile::TempDir,
    LooseObjectStore,
    ContentStore,
    LocalPublicationStore,
    RepositoryHead,
    ViewId,
) {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    for bytes in [b"one".as_slice(), b"two", b"three", b"child"] {
        content.put(bytes).unwrap();
    }
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    (dir, objects, content, heads, head, base)
}

#[tokio::test]
async fn squash_collapses_local_suffix_preserves_edits_and_publishes_with_one_cas() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let mut effective = base;
    let change = ChangeId::new();
    let first_edits = exact("file", None, b"one");
    let first_edit = *first_edits.edits.keys().next().unwrap();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, first) = mutator
        .create(
            head,
            base,
            effective,
            ChangeProposal {
                id: change,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: first_edits,
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    effective = first.resulting_view;
    let mut second_edits = exact("file", None, b"two");
    second_edits.transition.removed.insert(first_edit);
    let latest_edit = *second_edits.edits.keys().next().unwrap();
    let (prepared, second) = mutator
        .revise(
            head,
            base,
            effective,
            change,
            first.revision_id,
            Default::default(),
            second_edits,
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    effective = second.resulting_view;

    // Labels are authority references too: one exact rewritten target moves atomically while an
    // unaffected label and its imported-ref provenance remain byte-for-byte equivalent.
    let reader = RepositoryReader::new(&objects, &content);
    let current = reader.envelope(head.envelope).await.unwrap();
    let labels = ViewLabelManifestRecord::new([
        (
            "main",
            ViewLabel {
                view: effective,
                original_git_ref: Some("refs/heads/main".into()),
            },
        ),
        ("archive", base.into()),
    ])
    .unwrap();
    let labeled = RepositoryPublisher::new(&objects, &content, &heads)
        .prepare_envelope(current.state, &labels)
        .await
        .unwrap();
    head = RepositoryPublisher::new(&objects, &content, &heads)
        .publish_envelope(head, labeled)
        .await
        .unwrap();

    let authority_revision_set = RetainedSetStore::new(&objects)
        .update_batch(
            RetainedSetStore::new(&objects).empty_root().await.unwrap(),
            [(first.revision_id.into(), true)],
        )
        .await
        .unwrap();
    let request = SquashRewriteRequest {
        expected_revisions: [(change, second.revision_id)].into(),
        retained_predecessors: [(change, None)].into(),
        revision_metadata: [(
            change,
            RevisionMetadata {
                message: "squashed".into(),
                ..Default::default()
            },
        )]
        .into(),
        authority_roots: [base, effective].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: [authority_revision_set].into(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, effective, request)
        .await
        .unwrap()
        .unwrap();
    let replacement_id = prepared.mapping().revisions[&second.revision_id];
    assert_ne!(replacement_id, second.revision_id);
    let replacement = RepositoryReader::new(&objects, &content)
        .revision(replacement_id)
        .await
        .unwrap();
    let proposed_state = RepositoryReader::new(&objects, &content)
        .state(prepared.state())
        .await
        .unwrap();
    assert!(
        RetainedSetStore::new(&objects)
            .contains(proposed_state.retained_revisions, first.revision_id.into())
            .await
            .unwrap(),
        "exact external retained-revision authority survives even when no authority view selects it"
    );
    assert_eq!(replacement.predecessor, None);
    assert_eq!(replacement.edits, [latest_edit].into());
    assert!(replacement.transition.subsumed.is_empty());
    assert_eq!(
        prepared.mapping().views[&effective],
        prepared.resulting_view()
    );
    let prepared_resulting_view = prepared.resulting_view();
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    assert_eq!(published.generation, head.generation + 1);
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(published.envelope).await.unwrap();
    let rewritten_labels = reader.view_label_manifest(envelope.labels).await.unwrap();
    assert_eq!(
        rewritten_labels.labels["main"],
        ViewLabel {
            view: prepared_resulting_view,
            original_git_ref: Some("refs/heads/main".into()),
        }
    );
    assert_eq!(rewritten_labels.labels["archive"].view, base);
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_retains_published_boundary_rebuilds_descendant_and_rejects_stale_or_unknown_roots()
{
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let mut effective = base;
    let parent = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let first_edits = exact("file", None, b"one");
    let first_edit = *first_edits.edits.keys().next().unwrap();
    let (prepared, first) = mutator
        .create(
            head,
            base,
            effective,
            ChangeProposal {
                id: parent,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: first_edits,
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    effective = first.resulting_view;
    let mut second_edits = exact("file", None, b"two");
    let second_edit = *second_edits.edits.keys().next().unwrap();
    second_edits.transition.removed.insert(first_edit);
    second_edits.transition.subsumed.push(EditSubsumption {
        source: first_edit,
        replacements: [second_edit].into(),
    });
    let (prepared, second) = mutator
        .revise(
            head,
            base,
            effective,
            parent,
            first.revision_id,
            Default::default(),
            second_edits,
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    effective = second.resulting_view;
    let child = ChangeId::new();
    let (prepared, child_out) = mutator
        .create(
            head,
            base,
            effective,
            ChangeProposal {
                id: child,
                metadata: Default::default(),
                dependencies: [parent].into(),
                revision_metadata: Default::default(),
                edits: exact("child", None, b"child"),
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    effective = child_out.resulting_view;

    let expected = [(parent, second.revision_id), (child, child_out.revision_id)].into();
    let request = SquashRewriteRequest {
        expected_revisions: expected,
        retained_predecessors: [(parent, Some(first.revision_id))].into(),
        revision_metadata: [(parent, Default::default())].into(),
        authority_roots: [base, first.resulting_view].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, effective, request)
        .await
        .unwrap()
        .unwrap();
    let new_parent = prepared.mapping().revisions[&second.revision_id];
    let rewritten_parent = RepositoryReader::new(&objects, &content)
        .revision(new_parent)
        .await
        .unwrap();
    assert_eq!(rewritten_parent.predecessor, Some(first.revision_id));
    assert_eq!(rewritten_parent.transition.subsumed.len(), 1);
    assert_eq!(
        prepared.mapping().revisions[&child_out.revision_id],
        child_out.revision_id,
        "a descendant application is rebuilt without rewriting its clean authored revision"
    );

    let stale = SquashRewriteRequest {
        expected_revisions: BTreeMap::new(),
        retained_predecessors: [(parent, None)].into(),
        revision_metadata: [(parent, Default::default())].into(),
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(head, base, effective, stale)
            .await,
        Err(RepositoryRewriteError::StaleSelection)
    ));
}

#[tokio::test]
async fn rewrite_preserves_a_preexisting_conflicted_descendant_application() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let parent = ChangeId::new();
    let child = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let first_edits = exact("file", None, b"one");
    let first_edit = *first_edits.edits.keys().next().unwrap();
    let (prepared, first) = mutator
        .create(
            head,
            base,
            base,
            ChangeProposal {
                id: parent,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: first_edits,
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let (prepared, child_out) = mutator
        .create(
            head,
            base,
            first.resulting_view,
            ChangeProposal {
                id: child,
                metadata: Default::default(),
                dependencies: [parent].into(),
                revision_metadata: Default::default(),
                edits: exact("file", Some(b"one"), b"child"),
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let mut second_edits = exact("file", None, b"two");
    second_edits.transition.removed.insert(first_edit);
    let (prepared, second) = mutator
        .revise(
            head,
            base,
            child_out.resulting_view,
            parent,
            first.revision_id,
            Default::default(),
            second_edits,
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let before = reader.view(second.resulting_view).await.unwrap();
    let mut before_conflicts = ConflictSetStore::new(&objects)
        .cursor(before.unresolved_conflicts)
        .await
        .unwrap();
    assert!(before_conflicts.next().await.unwrap().is_some());

    let request = SquashRewriteRequest {
        expected_revisions: [(parent, second.revision_id), (child, child_out.revision_id)].into(),
        retained_predecessors: [(parent, Some(first.revision_id))].into(),
        revision_metadata: [(parent, Default::default())].into(),
        authority_roots: [base, first.resulting_view].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, second.resulting_view, request)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        prepared.mapping().revisions[&child_out.revision_id],
        child_out.revision_id
    );
    let after = reader.view(prepared.resulting_view()).await.unwrap();
    let mut after_conflicts = ConflictSetStore::new(&objects)
        .cursor(after.unresolved_conflicts)
        .await
        .unwrap();
    assert!(after_conflicts.next().await.unwrap().is_some());
}

#[tokio::test]
async fn packed_backend_reopen_preserves_prepared_rewrite_publication() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("packed");
    let heads = LocalPublicationStore::new(dir.path().join("head"));
    let objects = PackedObjectStore::open(&root).unwrap();
    let content = objects.content_store();
    content.put(b"one").unwrap();
    let repository = Repository::new(Tree::new());
    let base = repository.base_view().id;
    let mut head = RepositoryPublisher::new(&objects, &content, &heads)
        .initialize(&repository)
        .await
        .unwrap();
    let change = ChangeId::new();
    let (mutation, outcome) = PersistedRepositoryMutator::new(&objects, &content, &heads)
        .create(
            head,
            base,
            base,
            ChangeProposal {
                id: change,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: exact("file", None, b"one"),
            },
        )
        .await
        .unwrap();
    head = PersistedRepositoryMutator::new(&objects, &content, &heads)
        .publish(mutation)
        .await
        .unwrap();
    let request = SquashRewriteRequest {
        expected_revisions: [(change, outcome.revision_id)].into(),
        retained_predecessors: [(change, None)].into(),
        revision_metadata: [(
            change,
            RevisionMetadata {
                message: "packed rewrite".into(),
                ..Default::default()
            },
        )]
        .into(),
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, outcome.resulting_view, request)
        .await
        .unwrap()
        .unwrap();
    let proposed = prepared.proposed();
    finalize_repository_rewrite(&heads, prepared).unwrap();
    drop(content);
    drop(objects);

    let reopened = PackedObjectStore::open(&root).unwrap();
    let reopened_content = reopened.content_store();
    assert_eq!(heads.read().unwrap(), proposed);
    let envelope = RepositoryReader::new(&reopened, &reopened_content)
        .envelope(proposed.envelope)
        .await
        .unwrap();
    RepositoryReader::new(&reopened, &reopened_content)
        .validate_complete(envelope.state)
        .await
        .unwrap();
}

#[tokio::test]
async fn malformed_request_keys_and_current_boundary_are_typed_errors() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (prepared, outcome) = mutator
        .create(
            head,
            base,
            base,
            ChangeProposal {
                id: change,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: exact("file", None, b"one"),
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let expected: BTreeMap<ChangeId, RevisionId> = [(change, outcome.revision_id)].into();
    let request = |retained_predecessors, revision_metadata| SquashRewriteRequest {
        expected_revisions: expected.clone(),
        retained_predecessors,
        revision_metadata,
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let unknown = ChangeId::new();
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(
                head,
                base,
                outcome.resulting_view,
                request(
                    [(unknown, None)].into(),
                    [(unknown, Default::default())].into()
                ),
            )
            .await,
        Err(RepositoryRewriteError::UnknownRequestChange)
    ));
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(
                head,
                base,
                outcome.resulting_view,
                request([(change, None)].into(), BTreeMap::new()),
            )
            .await,
        Err(RepositoryRewriteError::MissingMetadata)
    ));
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(
                head,
                base,
                outcome.resulting_view,
                request(BTreeMap::new(), [(change, Default::default())].into()),
            )
            .await,
        Err(RepositoryRewriteError::UnexpectedMetadata)
    ));
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(
                head,
                base,
                outcome.resulting_view,
                request(
                    [(change, Some(outcome.revision_id))].into(),
                    [(change, Default::default())].into(),
                ),
            )
            .await,
        Err(RepositoryRewriteError::BoundaryIsCurrent)
    ));
    assert_eq!(heads.read().unwrap(), head);
}

#[tokio::test]
async fn squash_rejects_unrepresentable_subsumption_instead_of_dropping_it() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let first_edits = exact("file", None, b"one");
    let first_edit = *first_edits.edits.keys().next().unwrap();
    let (prepared, first) = mutator
        .create(
            head,
            base,
            base,
            ChangeProposal {
                id: change,
                metadata: Default::default(),
                dependencies: BTreeSet::new(),
                revision_metadata: Default::default(),
                edits: first_edits,
            },
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let mut second_edits = exact("file", None, b"two");
    let second_edit = *second_edits.edits.keys().next().unwrap();
    second_edits.transition.removed.insert(first_edit);
    second_edits.transition.subsumed.push(EditSubsumption {
        source: first_edit,
        replacements: [second_edit].into(),
    });
    let (prepared, second) = mutator
        .revise(
            head,
            base,
            first.resulting_view,
            change,
            first.revision_id,
            Default::default(),
            second_edits,
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();
    let request = SquashRewriteRequest {
        expected_revisions: [(change, second.revision_id)].into(),
        retained_predecessors: [(change, None)].into(),
        revision_metadata: [(change, Default::default())].into(),
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(head, base, second.resulting_view, request)
            .await,
        Err(RepositoryRewriteError::UnsupportedEvidence)
    ));
    assert_eq!(heads.read().unwrap(), head);
}

#[tokio::test]
async fn empty_rewrite_is_a_true_no_op() {
    let (_dir, objects, content, heads, head, base) = fixture().await;
    let request = SquashRewriteRequest {
        expected_revisions: BTreeMap::new(),
        retained_predecessors: BTreeMap::new(),
        revision_metadata: BTreeMap::new(),
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    assert!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(head, base, base, request)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(heads.read().unwrap(), head);
}
