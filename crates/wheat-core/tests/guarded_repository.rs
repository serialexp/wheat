use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};
use wheat_core::*;
#[derive(Default)]
struct Io {
    objects: RefCell<BTreeMap<ContentId, Vec<u8>>>,
    fail_store: Cell<bool>,
    resolves: Cell<usize>,
    stores: Cell<usize>,
}
impl Io {
    fn calls(&self) -> (usize, usize) {
        (self.resolves.get(), self.stores.get())
    }
}
impl ContentIo for Io {
    type Error = &'static str;
    fn resolve(&self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        self.resolves.set(self.resolves.get() + 1);
        let bytes = self.objects.borrow().get(&id).cloned().ok_or("missing")?;
        VerifiedContent::check(id, bytes).map_err(|_| "corrupt")
    }
    fn store_generated(&self, content: VerifiedContent<&[u8]>) -> Result<(), Self::Error> {
        self.stores.set(self.stores.get() + 1);
        if self.fail_store.get() {
            Err("sink")
        } else {
            self.objects
                .borrow_mut()
                .insert(content.id(), content.bytes().to_vec());
            Ok(())
        }
    }
}
fn exact(io: &Io, path: &str, before: Option<&[u8]>, after: Option<&[u8]>) -> RevisionEdits {
    let value = |bytes: &[u8]| {
        let content = ContentId::of(bytes);
        io.objects.borrow_mut().insert(content, bytes.to_vec());
        FileValue {
            content,
            kind: FileKind::Regular,
        }
    };
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: before.map(value),
            replacement: after.map(value),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: BTreeMap::from([(id, edit)]),
        transition: EditTransition {
            added: BTreeSet::from([id]),
            ..Default::default()
        },
    }
}

fn capture(io: &Io, before: &[u8], after: &[u8]) -> RevisionEdits {
    let c = edit_capture::capture_edit(
        "f",
        Some(edit_capture::FileSnapshot {
            kind: FileKind::Regular,
            bytes: before,
        }),
        Some(edit_capture::FileSnapshot {
            kind: FileKind::Regular,
            bytes: after,
        }),
        None,
        guarded::Limits::default(),
    )
    .unwrap();
    for (id, content) in &c.content {
        io.objects
            .borrow_mut()
            .insert(*id, content.bytes().to_vec());
    }
    let ids = c.edits.keys().copied().collect::<BTreeSet<_>>();
    RevisionEdits {
        edits: c.edits,
        transition: EditTransition {
            added: ids,
            ..Default::default()
        },
    }
}
#[test]
fn guarded_edit_moves_with_unique_context() {
    let io = Io::default();
    let before = b"a\nb\nc\nd\ne\nhead\nold\ntail\nf\ng\nh\ni\nj\n";
    let desired = b"a\nb\nc\nd\ne\nhead\nnew\ntail\nf\ng\nh\ni\nj\n";
    let edits = capture(&io, before, desired);
    let moved = b"prefix\na\nb\nc\nd\ne\nhead\nold\ntail\nf\ng\nh\ni\nj\n";
    let mv = FileValue {
        content: ContentId::of(moved),
        kind: FileKind::Regular,
    };
    io.objects.borrow_mut().insert(mv.content, moved.to_vec());
    let mut repo = Repository::new(Tree::from_iter([("f".into(), mv)]));
    let effective = repo.base_view().id;
    let (_, outcome) = repo
        .create_change(
            repo.base_view().id,
            effective,
            [],
            edits,
            ApplicationContext::new(&io),
        )
        .unwrap();
    let value = repo.view(outcome.resulting_view).unwrap().tree["f"];
    assert_eq!(
        io.resolve(value.content).unwrap().bytes(),
        b"prefix\na\nb\nc\nd\ne\nhead\nnew\ntail\nf\ng\nh\ni\nj\n"
    );
}
#[test]
fn failed_generated_sink_rolls_back_metadata() {
    let io = Io::default();
    let before = b"a\nold\nz\n";
    let edits = capture(&io, before, b"a\nnew\nz\n");
    let base = FileValue {
        content: ContentId::of(before),
        kind: FileKind::Regular,
    };
    let mut repo = Repository::new(Tree::from_iter([("f".into(), base)]));
    let effective = repo.base_view().id;
    let prior = repo.canonical_closure();
    io.fail_store.set(true);
    assert!(
        repo.create_change(
            repo.base_view().id,
            effective,
            [],
            edits,
            ApplicationContext::new(&io)
        )
        .is_err()
    );
    assert_eq!(repo.canonical_closure(), prior);
}

#[test]
fn invalid_initial_transitions_are_rejected_before_content_io() {
    let cases = ["preserved", "removed", "subsumed", "resolution"];
    for case in cases {
        let io = Io::default();
        let mut repo = Repository::new(Tree::new());
        let effective = repo.base_view().id;
        let prior = repo.canonical_closure();
        let mut edits = exact(&io, "f", None, Some(b"new"));
        let id = *edits.edits.keys().next().unwrap();
        match case {
            "preserved" => {
                edits.transition.added.clear();
                edits.transition.preserved.insert(id);
            }
            "removed" => {
                edits
                    .transition
                    .removed
                    .insert(EditId::from_digest([1; 32]));
            }
            "subsumed" => edits.transition.subsumed.push(EditSubsumption {
                source: EditId::from_digest([2; 32]),
                replacements: BTreeSet::from([id]),
            }),
            "resolution" => edits
                .transition
                .resolutions
                .push(ConflictResolutionEvidence {
                    application: ApplicationId::from_digest([3; 32]),
                    conflicted_edits: BTreeSet::from([id]),
                    replacement_edits: BTreeSet::from([id]),
                }),
            _ => unreachable!(),
        }
        let reason = if case == "subsumed" {
            "invalid local edit selection"
        } else {
            "initial revision has predecessor lineage"
        };
        assert_eq!(
            repo.create_change(
                repo.base_view().id,
                effective,
                [],
                edits,
                ApplicationContext::new(&io)
            ),
            Err(RepositoryError::InvalidEditTransition(reason))
        );
        assert_eq!(io.calls(), (0, 0), "{case}");
        assert_eq!(repo.canonical_closure(), prior, "{case}");
    }
}

#[test]
fn malformed_edit_key_and_local_transition_are_preflighted() {
    let io = Io::default();
    let mut repo = Repository::new(Tree::new());
    let effective = repo.base_view().id;
    let prior = repo.canonical_closure();
    let mut malformed = exact(&io, "f", None, Some(b"new"));
    let (_, edit) = malformed.edits.pop_first().unwrap();
    malformed.edits.insert(EditId::from_digest([4; 32]), edit);
    assert_eq!(
        repo.create_change(
            repo.base_view().id,
            effective,
            [],
            malformed,
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "invalid local edit selection"
        ))
    );
    let mut local = exact(&io, "f", None, Some(b"new"));
    let id = *local.edits.keys().next().unwrap();
    local.transition.added.clear();
    local.transition.preserved.insert(id);
    assert_eq!(
        repo.create_change(
            repo.base_view().id,
            effective,
            [],
            local,
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "initial revision has predecessor lineage"
        ))
    );
    assert_eq!(io.calls(), (0, 0));
    assert_eq!(repo.canonical_closure(), prior);
}

#[test]
fn revision_transition_preflight_rejects_false_partition_and_preserved_key() {
    let io = Io::default();
    let mut repo = Repository::new(Tree::new());
    let effective = repo.base_view().id;
    let (change, outcome) = repo
        .create_change(
            repo.base_view().id,
            effective,
            [],
            exact(&io, "f", None, Some(b"old")),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let effective = outcome.resulting_view;
    let prior = repo.canonical_closure();
    let old = *repo
        .revision(outcome.revision_id)
        .unwrap()
        .edits()
        .keys()
        .next()
        .unwrap();
    let mut partition = exact(&io, "f", Some(b"old"), Some(b"new"));
    let replacement = *partition.edits.keys().next().unwrap();
    partition.transition.added.clear();
    partition.transition.preserved.insert(replacement);
    assert_eq!(
        repo.revise_change(
            repo.base_view().id,
            effective,
            change,
            outcome.revision_id,
            partition,
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "predecessor edit partition"
        ))
    );
    let mut preserved = exact(&io, "f", Some(b"old"), Some(b"new"));
    let replacement = *preserved.edits.keys().next().unwrap();
    preserved.transition.added.clear();
    preserved.transition.preserved.insert(replacement);
    preserved.transition.removed.insert(old);
    assert_eq!(
        repo.revise_change(
            repo.base_view().id,
            effective,
            change,
            outcome.revision_id,
            preserved,
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "predecessor edit partition"
        ))
    );
    // A changed record under a retained key is rejected by local identity validation,
    // before the predecessor-definition comparison can trust that key.
    let mut changed_key = exact(&io, "f", Some(b"old"), Some(b"new"));
    let (_, changed) = changed_key.edits.pop_first().unwrap();
    changed_key.edits.insert(old, changed);
    changed_key.transition.added.clear();
    changed_key.transition.preserved.insert(old);
    assert_eq!(
        repo.revise_change(
            repo.base_view().id,
            effective,
            change,
            outcome.revision_id,
            changed_key,
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "invalid local edit selection"
        ))
    );
    assert_eq!(repo.canonical_closure(), prior);
}

#[test]
fn invalid_resolution_evidence_is_rejected_before_content_io() {
    let io = Io::default();
    let mut repo = Repository::new(Tree::new());
    let effective = repo.base_view().id;
    let (change, outcome) = repo
        .create_change(
            repo.base_view().id,
            effective,
            [],
            exact(&io, "f", None, Some(b"old")),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let old = *repo
        .revision(outcome.revision_id)
        .unwrap()
        .edits()
        .keys()
        .next()
        .unwrap();
    let clean_application = repo
        .current_application(outcome.resulting_view, change)
        .unwrap()
        .id;
    let (other_change, other_outcome) = repo
        .create_change(
            repo.base_view().id,
            outcome.resulting_view,
            [],
            exact(&io, "g", None, Some(b"other")),
            ApplicationContext::new(&io),
        )
        .unwrap();
    let effective = other_outcome.resulting_view;
    let wrong_change_application = repo
        .current_application(effective, other_change)
        .unwrap()
        .id;
    let prior = repo.canonical_closure();

    for (application, reason) in [
        (
            ApplicationId::from_digest([9; 32]),
            "resolution application is not retained",
        ),
        (
            wrong_change_application,
            "resolution application does not match the predecessor",
        ),
        (
            clean_application,
            "resolution application is not conflicted",
        ),
    ] {
        let mut edits = exact(&io, "f", Some(b"old"), Some(b"new"));
        let replacement = *edits.edits.keys().next().unwrap();
        edits.transition.removed.insert(old);
        edits
            .transition
            .resolutions
            .push(ConflictResolutionEvidence {
                application,
                conflicted_edits: BTreeSet::from([old]),
                replacement_edits: BTreeSet::from([replacement]),
            });
        let calls = io.calls();
        assert_eq!(
            repo.revise_change(
                repo.base_view().id,
                effective,
                change,
                outcome.revision_id,
                edits,
                ApplicationContext::new(&io)
            ),
            Err(RepositoryError::InvalidEditTransition(reason))
        );
        assert_eq!(io.calls(), calls);
        assert_eq!(repo.canonical_closure(), prior);
    }
}

#[test]
fn invalid_import_batch_does_no_content_io_or_mutate_repository() {
    let io = Io::default();
    let mut repo = Repository::new(Tree::new());
    let effective = repo.base_view().id;
    let prior = repo.canonical_closure();
    let valid = ChangeProposal {
        id: ChangeId::new(),
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits: exact(&io, "good", None, Some(b"good")),
    };
    let mut invalid_edits = exact(&io, "bad", None, Some(b"bad"));
    let id = *invalid_edits.edits.keys().next().unwrap();
    invalid_edits.transition.added.clear();
    invalid_edits.transition.preserved.insert(id);
    let invalid = ChangeProposal {
        id: ChangeId::new(),
        metadata: ChangeMetadata::default(),
        dependencies: BTreeSet::new(),
        revision_metadata: RevisionMetadata::default(),
        edits: invalid_edits,
    };
    assert_eq!(
        repo.import_changes(
            repo.base_view().id,
            effective,
            [valid, invalid],
            ApplicationContext::new(&io)
        ),
        Err(RepositoryError::InvalidEditTransition(
            "initial revision has predecessor lineage"
        ))
    );
    assert_eq!(io.calls(), (0, 0));
    assert_eq!(repo.canonical_closure(), prior);
}
