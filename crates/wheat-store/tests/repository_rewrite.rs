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

fn file_edit(path: &str, expected: Option<&[u8]>, replacement: &[u8]) -> EditRecord {
    exact(path, expected, replacement)
        .edits
        .into_values()
        .next()
        .unwrap()
}

/// A revision selecting `kept` (already selected by its predecessor) plus `added`, removing
/// `removed`, with explicit `source -> replacements` subsumption evidence.
fn transition(
    kept: &[&EditRecord],
    added: &[&EditRecord],
    removed: &[&EditRecord],
    subsumed: &[(&EditRecord, &[&EditRecord])],
) -> RevisionEdits {
    let ids = |edits: &[&EditRecord]| edits.iter().map(|e| e.id()).collect::<BTreeSet<_>>();
    RevisionEdits {
        edits: kept
            .iter()
            .chain(added)
            .map(|edit| (edit.id(), (*edit).clone()))
            .collect(),
        transition: EditTransition {
            preserved: ids(kept),
            added: ids(added),
            removed: ids(removed),
            subsumed: subsumed
                .iter()
                .map(|(source, replacements)| EditSubsumption {
                    source: source.id(),
                    replacements: ids(replacements),
                })
                .collect(),
            resolutions: Vec::new(),
        },
    }
}

/// Creates one change from `revisions`, publishing each, and returns every outcome.
async fn change_history(
    mutator: &PersistedRepositoryMutator<'_, LooseObjectStore, LocalPublicationStore>,
    head: &mut RepositoryHead,
    base: ViewId,
    change: ChangeId,
    revisions: Vec<RevisionEdits>,
) -> Vec<RevisionOutcome> {
    let mut outcomes: Vec<RevisionOutcome> = Vec::new();
    for edits in revisions {
        let (prepared, outcome) = match outcomes.last() {
            None => mutator
                .create(
                    *head,
                    base,
                    base,
                    ChangeProposal {
                        id: change,
                        metadata: Default::default(),
                        dependencies: BTreeSet::new(),
                        revision_metadata: Default::default(),
                        edits,
                    },
                )
                .await
                .unwrap(),
            Some(previous) => mutator
                .revise(
                    *head,
                    base,
                    previous.resulting_view,
                    change,
                    previous.revision_id,
                    Default::default(),
                    edits,
                )
                .await
                .unwrap(),
        };
        *head = mutator.publish(prepared).await.unwrap();
        outcomes.push(outcome);
    }
    outcomes
}

fn squash_one(
    change: ChangeId,
    latest: RevisionId,
    boundary: Option<RevisionId>,
    roots: BTreeSet<ViewId>,
) -> SquashRewriteRequest {
    SquashRewriteRequest {
        expected_revisions: [(change, latest)].into(),
        retained_predecessors: [(change, boundary)].into(),
        revision_metadata: [(change, Default::default())].into(),
        authority_roots: roots,
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    }
}

#[tokio::test]
async fn squash_of_an_unpublished_change_edited_twice_drops_intermediate_subsumption() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let one = file_edit("file", None, b"one");
    let two = file_edit("file", None, b"two");
    let history = change_history(
        &mutator,
        &mut head,
        base,
        change,
        vec![
            transition(&[], &[&one], &[], &[]),
            transition(&[], &[&two], &[&one], &[(&one, &[&two])]),
        ],
    )
    .await;
    let latest = &history[1];
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(
            head,
            base,
            latest.resulting_view,
            squash_one(change, latest.revision_id, None, [base].into()),
        )
        .await
        .unwrap()
        .unwrap();
    let replacement = RepositoryReader::new(&objects, &content)
        .revision(prepared.mapping().revisions[&latest.revision_id])
        .await
        .unwrap();
    assert_eq!(replacement.predecessor, None);
    assert_eq!(replacement.edits, [two.id()].into());
    assert_eq!(
        replacement.transition,
        EditTransition {
            added: [two.id()].into(),
            ..Default::default()
        },
        "an initial revision has no predecessor for the intermediate edit to be evidence about"
    );
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_composes_a_subsumption_chain_at_the_published_boundary() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let one = file_edit("file", None, b"one");
    let two = file_edit("file", None, b"two");
    let three = file_edit("file", None, b"three");
    let history = change_history(
        &mutator,
        &mut head,
        base,
        change,
        vec![
            transition(&[], &[&one], &[], &[]),
            transition(&[], &[&two], &[&one], &[(&one, &[&two])]),
            transition(&[], &[&three], &[&two], &[(&two, &[&three])]),
        ],
    )
    .await;
    let (boundary, latest) = (&history[0], &history[2]);
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(
            head,
            base,
            latest.resulting_view,
            squash_one(
                change,
                latest.revision_id,
                Some(boundary.revision_id),
                [base, boundary.resulting_view].into(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    let replacement = RepositoryReader::new(&objects, &content)
        .revision(prepared.mapping().revisions[&latest.revision_id])
        .await
        .unwrap();
    assert_eq!(replacement.predecessor, Some(boundary.revision_id));
    assert_eq!(
        replacement.transition,
        EditTransition {
            added: [three.id()].into(),
            removed: [one.id()].into(),
            subsumed: vec![EditSubsumption {
                source: one.id(),
                replacements: [three.id()].into(),
            }],
            ..Default::default()
        }
    );
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_drops_evidence_for_a_readded_source_or_a_removed_replacement() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let file_one = file_edit("file", None, b"one");
    let file_two = file_edit("file", None, b"two");
    let other_one = file_edit("other", None, b"one");
    let other_two = file_edit("other", None, b"two");
    let history = change_history(
        &mutator,
        &mut head,
        base,
        change,
        vec![
            transition(&[], &[&file_one, &other_one], &[], &[]),
            transition(
                &[],
                &[&file_two, &other_two],
                &[&file_one, &other_one],
                &[(&file_one, &[&file_two]), (&other_one, &[&other_two])],
            ),
            // `file_one` comes back and `other_two` is dropped without a replacement.
            transition(&[], &[&file_one], &[&file_two, &other_two], &[]),
        ],
    )
    .await;
    let (boundary, latest) = (&history[0], &history[2]);
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(
            head,
            base,
            latest.resulting_view,
            squash_one(
                change,
                latest.revision_id,
                Some(boundary.revision_id),
                [base, boundary.resulting_view].into(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
    let replacement = RepositoryReader::new(&objects, &content)
        .revision(prepared.mapping().revisions[&latest.revision_id])
        .await
        .unwrap();
    assert_eq!(
        replacement.transition,
        EditTransition {
            preserved: [file_one.id()].into(),
            removed: [other_one.id()].into(),
            ..Default::default()
        }
    );
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_still_rejects_evidence_whose_replacement_is_a_boundary_edit() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let change = ChangeId::new();
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let file_one = file_edit("file", None, b"one");
    let file_two = file_edit("file", None, b"two");
    let other = file_edit("other", None, b"one");
    let history = change_history(
        &mutator,
        &mut head,
        base,
        change,
        vec![
            transition(&[], &[&file_one, &other], &[], &[]),
            transition(
                &[],
                &[&file_two],
                &[&file_one, &other],
                &[(&file_one, &[&file_two])],
            ),
            // The composed replacement of `file_one` is `other`, which the boundary already
            // selects, so `file_one -> {other}` is not a direct transition from the boundary.
            transition(&[], &[&other], &[&file_two], &[(&file_two, &[&other])]),
        ],
    )
    .await;
    let (boundary, latest) = (&history[0], &history[2]);
    assert!(matches!(
        RepositoryRewriter::new(&objects, &content, &heads)
            .prepare_squash(
                head,
                base,
                latest.resulting_view,
                squash_one(
                    change,
                    latest.revision_id,
                    Some(boundary.revision_id),
                    [base, boundary.resulting_view].into(),
                ),
            )
            .await,
        Err(RepositoryRewriteError::UnsupportedEvidence)
    ));
    assert_eq!(heads.read().unwrap(), head);
}

/// Parent `file: one`, child `file: one -> child`, then the parent is revised to `file: two`,
/// which conflicts the child. Returns the child's conflicted revision and application.
async fn conflicted_child(
    mutator: &PersistedRepositoryMutator<'_, LooseObjectStore, LocalPublicationStore>,
    objects: &LooseObjectStore,
    content: &ContentStore,
    head: &mut RepositoryHead,
    base: ViewId,
    parent: ChangeId,
    child: ChangeId,
) -> (RevisionOutcome, RevisionOutcome, ApplicationId) {
    let parent_one = file_edit("file", None, b"one");
    let parent_history = change_history(
        mutator,
        head,
        base,
        parent,
        vec![transition(&[], &[&parent_one], &[], &[])],
    )
    .await;
    let (prepared, child_first) = mutator
        .create(
            *head,
            base,
            parent_history[0].resulting_view,
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
    *head = mutator.publish(prepared).await.unwrap();
    let parent_two = file_edit("file", None, b"two");
    let (prepared, parent_second) = mutator
        .revise(
            *head,
            base,
            child_first.resulting_view,
            parent,
            parent_history[0].revision_id,
            Default::default(),
            transition(&[], &[&parent_two], &[&parent_one], &[]),
        )
        .await
        .unwrap();
    *head = mutator.publish(prepared).await.unwrap();
    let view = RepositoryReader::new(objects, content)
        .view(parent_second.resulting_view)
        .await
        .unwrap();
    let conflicted = ApplicationMapStore::new(objects)
        .get(view.applications, child)
        .await
        .unwrap()
        .unwrap();
    (child_first, parent_second, conflicted)
}

async fn resolve_child(
    mutator: &PersistedRepositoryMutator<'_, LooseObjectStore, LocalPublicationStore>,
    head: &mut RepositoryHead,
    base: ViewId,
    effective: ViewId,
    conflict_revision: RevisionId,
    application: ApplicationId,
) -> RevisionOutcome {
    let resolved = FileValue {
        content: ContentId::of(b"child"),
        kind: FileKind::Regular,
    };
    let (prepared, outcome) = mutator
        .resolve_conflict(
            *head,
            base,
            effective,
            PersistedConflictResolution {
                conflict_revision,
                application,
                resolved: [("file".into(), Some(resolved))].into(),
            },
        )
        .await
        .unwrap();
    *head = mutator.publish(prepared).await.unwrap();
    outcome
}

#[tokio::test]
async fn squash_drops_a_resolution_of_an_intermediate_checkpoint() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let (parent, child) = (ChangeId::new(), ChangeId::new());
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (child_first, parent_second, conflicted) =
        conflicted_child(&mutator, &objects, &content, &mut head, base, parent, child).await;
    let resolved = resolve_child(
        &mutator,
        &mut head,
        base,
        parent_second.resulting_view,
        child_first.revision_id,
        conflicted,
    )
    .await;
    let reader = RepositoryReader::new(&objects, &content);
    assert_eq!(
        reader
            .revision(resolved.revision_id)
            .await
            .unwrap()
            .transition
            .resolutions
            .len(),
        1
    );

    let request = SquashRewriteRequest {
        expected_revisions: [
            (parent, parent_second.revision_id),
            (child, resolved.revision_id),
        ]
        .into(),
        retained_predecessors: [(child, None)].into(),
        revision_metadata: [(child, Default::default())].into(),
        authority_roots: [base].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, resolved.resulting_view, request)
        .await
        .unwrap()
        .unwrap();
    let replacement = reader
        .revision(prepared.mapping().revisions[&resolved.revision_id])
        .await
        .unwrap();
    assert_eq!(replacement.predecessor, None);
    assert!(replacement.transition.resolutions.is_empty());
    let after = reader.view(prepared.resulting_view()).await.unwrap();
    assert!(
        ConflictSetStore::new(&objects)
            .cursor(after.unresolved_conflicts)
            .await
            .unwrap()
            .next()
            .await
            .unwrap()
            .is_none()
    );
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
}

#[tokio::test]
async fn squash_keeps_a_resolution_of_a_conflict_at_the_boundary() {
    let (_dir, objects, content, heads, mut head, base) = fixture().await;
    let (parent, child) = (ChangeId::new(), ChangeId::new());
    let mutator = PersistedRepositoryMutator::new(&objects, &content, &heads);
    let (child_first, parent_second, conflicted) =
        conflicted_child(&mutator, &objects, &content, &mut head, base, parent, child).await;
    let resolved = resolve_child(
        &mutator,
        &mut head,
        base,
        parent_second.resulting_view,
        child_first.revision_id,
        conflicted,
    )
    .await;
    let reader = RepositoryReader::new(&objects, &content);
    let resolved_record = reader.revision(resolved.revision_id).await.unwrap();
    let mut kept = Vec::new();
    for id in &resolved_record.edits {
        kept.push(reader.edit(*id).await.unwrap());
    }
    let kept = kept.iter().collect::<Vec<_>>();
    let extra = file_edit("other", None, b"three");
    let (prepared, latest) = mutator
        .revise(
            head,
            base,
            resolved.resulting_view,
            child,
            resolved.revision_id,
            Default::default(),
            transition(&kept, &[&extra], &[], &[]),
        )
        .await
        .unwrap();
    head = mutator.publish(prepared).await.unwrap();

    let request = SquashRewriteRequest {
        expected_revisions: [
            (parent, parent_second.revision_id),
            (child, latest.revision_id),
        ]
        .into(),
        retained_predecessors: [(child, Some(child_first.revision_id))].into(),
        revision_metadata: [(child, Default::default())].into(),
        authority_roots: [base, child_first.resulting_view].into(),
        authority_view_sets: BTreeSet::new(),
        authority_revision_sets: BTreeSet::new(),
    };
    let prepared = RepositoryRewriter::new(&objects, &content, &heads)
        .prepare_squash(head, base, latest.resulting_view, request)
        .await
        .unwrap()
        .unwrap();
    let replacement = reader
        .revision(prepared.mapping().revisions[&latest.revision_id])
        .await
        .unwrap();
    assert_eq!(replacement.predecessor, Some(child_first.revision_id));
    assert_eq!(
        replacement.transition.resolutions, resolved_record.transition.resolutions,
        "the boundary revision's conflict is still resolved by the collapsed revision"
    );
    let published = finalize_repository_rewrite(&heads, prepared).unwrap();
    let envelope = reader.envelope(published.envelope).await.unwrap();
    reader.validate_complete(envelope.state).await.unwrap();
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
