//! Incremental validation: every step of a built history is a valid increment on the
//! one before, and states forged from a valid one are refused for what they forge.

use wheat_core::*;
use wheat_store::*;

fn file(bytes: &[u8]) -> FileValue {
    FileValue {
        content: ContentId::of(bytes),
        kind: FileKind::Regular,
    }
}

/// A file write: path, the content expected there (`None`: no file), replacement.
type Write<'a> = (&'a str, Option<&'a [u8]>, &'a [u8]);

/// Edits making each write, all added.
fn files(entries: &[Write<'_>]) -> RevisionEdits {
    let mut edits = RevisionEdits::default();
    for (path, expected, replacement) in entries {
        let edit = EditRecord::new(
            *path,
            EditOperation::ExactFile {
                expected: expected.map(file),
                replacement: Some(file(replacement)),
            },
        )
        .unwrap();
        edits.transition.added.insert(edit.id());
        edits.edits.insert(edit.id(), edit);
    }
    edits
}

fn change(byte: u8) -> ChangeId {
    ChangeId::from_bytes([byte; 16])
}

/// A repository built by the mutator, with the envelope each publication left.
struct History {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    base: ViewId,
    /// The published envelopes, oldest first.
    envelopes: Vec<RepositoryEnvelopeId>,
}

impl History {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        let head = RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap();
        Self {
            _dir: dir,
            objects,
            content,
            heads,
            base,
            envelopes: vec![head.envelope],
        }
    }

    fn mutator(&self) -> PersistedRepositoryMutator<'_, LooseObjectStore, LocalPublicationStore> {
        PersistedRepositoryMutator::new(&self.objects, &self.content, &self.heads)
    }

    fn reader(&self) -> RepositoryReader<'_, LooseObjectStore> {
        RepositoryReader::new(&self.objects, &self.content)
    }

    fn head(&self) -> RepositoryHead {
        self.heads.read().unwrap()
    }

    async fn publish(&mut self, prepared: PreparedRepositoryMutation) {
        let head = self.mutator().publish(prepared).await.unwrap();
        self.envelopes.push(head.envelope);
    }

    /// Creates `change` on `effective` with `edits`, their content already stored.
    async fn create(
        &mut self,
        effective: ViewId,
        change: ChangeId,
        dependencies: &[ChangeId],
        edits: RevisionEdits,
    ) -> (ViewId, RevisionId) {
        let (prepared, outcome) = self
            .mutator()
            .create(
                self.head(),
                self.base,
                effective,
                ChangeProposal {
                    id: change,
                    metadata: ChangeMetadata::default(),
                    dependencies: dependencies.iter().copied().collect(),
                    revision_metadata: RevisionMetadata::default(),
                    edits,
                },
            )
            .await
            .unwrap();
        self.publish(prepared).await;
        (outcome.resulting_view, outcome.revision_id)
    }

    /// Revises `change` from `predecessor` to `edits` alone, their content already
    /// stored.
    async fn replace(
        &mut self,
        effective: ViewId,
        change: ChangeId,
        predecessor: RevisionId,
        mut edits: RevisionEdits,
    ) -> ViewId {
        edits.transition.removed = self.reader().revision(predecessor).await.unwrap().edits;
        let (prepared, outcome) = self
            .mutator()
            .revise(
                self.head(),
                self.base,
                effective,
                change,
                predecessor,
                RevisionMetadata::default(),
                edits,
            )
            .await
            .unwrap();
        self.publish(prepared).await;
        outcome.resulting_view
    }

    /// Revises `change` from `predecessor`, keeping its edits and adding one writing
    /// `path` (whose content is already stored), committed at `at`.
    async fn add(
        &mut self,
        effective: ViewId,
        change: ChangeId,
        predecessor: RevisionId,
        path: &str,
        at: i64,
    ) -> (ViewId, RevisionId) {
        let mut edits = files(&[(path, None, path.as_bytes())]);
        for id in self.reader().revision(predecessor).await.unwrap().edits {
            edits
                .edits
                .insert(id, self.reader().edit(id).await.unwrap());
            edits.transition.preserved.insert(id);
        }
        let metadata = RevisionMetadata {
            committed_at: Timestamp {
                seconds: at,
                offset_minutes: 0,
            },
            ..RevisionMetadata::default()
        };
        let (prepared, outcome) = self
            .mutator()
            .revise(
                self.head(),
                self.base,
                effective,
                change,
                predecessor,
                metadata,
                edits,
            )
            .await
            .unwrap();
        self.publish(prepared).await;
        (outcome.resulting_view, outcome.revision_id)
    }

    /// The published state with `revision` stored and retained.
    async fn retaining_revision(&self, revision: &RevisionRecord) -> RepositoryStateRecord {
        self.put(revision.id().into(), &revision.canonical_bytes())
            .await;
        let state = self.state().await;
        RepositoryStateRecord {
            retained_revisions: RetainedSetStore::new(&self.objects)
                .insert(state.retained_revisions, revision.id().into())
                .await
                .unwrap(),
            ..state
        }
    }

    async fn application_in(&self, view: ViewId, change: ChangeId) -> ApplicationRecord {
        let view = self.reader().view(view).await.unwrap();
        let id = ApplicationMapStore::new(&self.objects)
            .get(view.applications, change)
            .await
            .unwrap()
            .unwrap();
        self.reader().application(id).await.unwrap()
    }

    async fn state_of(&self, envelope: RepositoryEnvelopeId) -> RepositoryStateRecord {
        let reader = self.reader();
        reader
            .state(reader.envelope(envelope).await.unwrap().state)
            .await
            .unwrap()
    }

    async fn state(&self) -> RepositoryStateRecord {
        self.state_of(self.head().envelope).await
    }

    async fn put(&self, id: ObjectId, bytes: &[u8]) {
        self.objects.put_if_absent(id, bytes).await.unwrap();
    }

    /// Stores `state` in an envelope with `old`'s labels.
    async fn envelope(
        &self,
        old: RepositoryEnvelopeId,
        state: RepositoryStateRecord,
    ) -> RepositoryEnvelopeId {
        self.put(state.id().into(), &state.canonical_bytes()).await;
        let envelope = RepositoryEnvelopeRecord {
            state: state.id(),
            labels: self.reader().envelope(old).await.unwrap().labels,
        };
        self.put(envelope.id().into(), &envelope.canonical_bytes())
            .await;
        envelope.id()
    }

    /// Validates `state` as an increment on the published envelope and returns why it
    /// is refused, checking full validation refuses it too.
    async fn refusal(&self, state: RepositoryStateRecord) -> String {
        let published = self.head().envelope;
        let envelope = self.envelope(published, state).await;
        let reader = self.reader();
        let reason = reader
            .validate_increment(published, envelope, IncrementBudget::STANDARD)
            .await
            .expect_err("the increment is refused")
            .to_string();
        assert!(
            reader.validate_complete(state.id()).await.is_err(),
            "full validation accepts what the increment refuses ({reason})"
        );
        assert!(
            reader.validate_bootstrap(envelope).await.is_err(),
            "bootstrap accepts what the increment refuses ({reason})"
        );
        reason
    }

    /// The published state with `view` also retained, as a lineage child of `parent`.
    async fn retaining_view(&self, view: &ViewRecord, parent: ViewId) -> RepositoryStateRecord {
        self.put(view.id().into(), &view.canonical_bytes()).await;
        let state = self.state().await;
        RepositoryStateRecord {
            retained_views: RetainedSetStore::new(&self.objects)
                .insert(state.retained_views, view.id().into())
                .await
                .unwrap(),
            view_lineage: ViewLineageMapStore::new(&self.objects)
                .insert_edges(state.view_lineage, [(parent, view.id())])
                .await
                .unwrap(),
            ..state
        }
    }
}

/// `p` writes `f` and `g`, `c` on top rewrites both; revising `p` makes `c` conflict,
/// `c`'s conflict is resolved and `p` revised again. Then `x` and `y` write one path on
/// the base, and are integrated into a view where the later one conflicts.
struct Built {
    history: History,
    with_p: ViewId,
    /// The view where `c` conflicts after `p`'s first revision.
    conflicted: ViewId,
    c: ChangeId,
}

impl Built {
    async fn new() -> Self {
        let mut history = History::new().await;
        for bytes in [
            &b"f0"[..],
            b"g0",
            b"cf",
            b"cg",
            b"f1",
            b"g1",
            b"f2",
            b"g2",
            b"rf",
            b"rg",
            b"left",
            b"right",
        ] {
            history.content.put(bytes).unwrap();
        }
        let base = history.base;
        let (p, c) = (change(0x40), change(0x50));
        let (with_p, p1) = history
            .create(
                base,
                p,
                &[],
                files(&[("f", None, b"f0"), ("g", None, b"g0")]),
            )
            .await;
        let (with_c, _) = history
            .create(
                with_p,
                c,
                &[],
                files(&[("f", Some(b"f0"), b"cf"), ("g", Some(b"g0"), b"cg")]),
            )
            .await;
        let conflicted = history
            .replace(
                with_c,
                p,
                p1,
                files(&[("f", None, b"f1"), ("g", None, b"g1")]),
            )
            .await;
        let first_conflict = history.application_in(conflicted, c).await;
        let (prepared, resolved) = history
            .mutator()
            .resolve_conflict(
                history.head(),
                base,
                conflicted,
                PersistedConflictResolution {
                    conflict_revision: first_conflict.revision_id,
                    application: first_conflict.id(),
                    resolved: [
                        ("f".to_owned(), Some(file(b"rf"))),
                        ("g".to_owned(), Some(file(b"rg"))),
                    ]
                    .into(),
                },
            )
            .await
            .unwrap();
        history.publish(prepared).await;
        let p2 = history.application_in(resolved.resulting_view, p).await;
        history
            .replace(
                resolved.resulting_view,
                p,
                p2.revision_id,
                files(&[("f", None, b"f2"), ("g", None, b"g2")]),
            )
            .await;
        let (left, _) = history
            .create(base, change(0x60), &[], files(&[("same", None, b"left")]))
            .await;
        let (right, _) = history
            .create(base, change(0x70), &[], files(&[("same", None, b"right")]))
            .await;
        let (prepared, joined) = history
            .mutator()
            .integrate(history.head(), base, &[left, right])
            .await
            .unwrap();
        history.publish(prepared).await;
        assert_eq!(joined.conflicted.len(), 1);
        Self {
            history,
            with_p,
            conflicted,
            c,
        }
    }
}

/// Every publication of a history with branching, conflicts, a resolution, revisions
/// and an integration is a valid increment on the one before, and on the first.
#[tokio::test]
async fn every_publication_of_a_built_history_is_a_valid_increment() {
    let built = Built::new().await;
    let history = &built.history;
    let reader = history.reader();
    for step in history.envelopes.windows(2) {
        reader
            .validate_increment(step[0], step[1], IncrementBudget::STANDARD)
            .await
            .unwrap();
    }
    for envelope in &history.envelopes {
        reader.validate_bootstrap(*envelope).await.unwrap();
    }
    let (first, last) = (history.envelopes[0], *history.envelopes.last().unwrap());
    let report = reader
        .validate_increment(first, last, IncrementBudget::STANDARD)
        .await
        .unwrap();
    assert!(report.views >= 8 && report.applications >= 8, "{report:?}");
    assert_eq!(report.changes, 4);
    reader
        .validate_complete(history.state().await.id())
        .await
        .unwrap();
}

/// One change created on the base adds what the mutator wrote, and nothing old.
#[tokio::test]
async fn a_created_change_reports_what_it_added() {
    let mut history = History::new().await;
    history.content.put(b"a").unwrap();
    let base = history.base;
    history
        .create(base, change(1), &[], files(&[("a", None, b"a")]))
        .await;
    let report = history
        .reader()
        .validate_increment(
            history.envelopes[0],
            history.envelopes[1],
            IncrementBudget::STANDARD,
        )
        .await
        .unwrap();
    // Six objects, and the one edit replayed by the revision and the application.
    assert_eq!(report.budget_units, 8);
    assert!(report.budget_spent <= report.budget_allowance, "{report:?}");
    assert_eq!(
        report,
        IncrementReport {
            changes: 1,
            views: 1,
            revisions: 1,
            applications: 1,
            edges: 1,
            content: 1,
            labels: 0,
            budget_units: 8,
            budget_allowance: report.budget_allowance,
            budget_spent: report.budget_spent,
        }
    );
}

/// The forgery that motivated this validator: a view copying another view's revision
/// map over the base's empty application map and tree.
#[tokio::test]
async fn a_view_copying_another_views_revision_map_is_refused() {
    let built = Built::new().await;
    let history = &built.history;
    let reader = history.reader();
    let with_p = reader.view(built.with_p).await.unwrap();
    let base = reader.view(history.base).await.unwrap();
    let forged = ViewRecord {
        revisions: with_p.revisions,
        order: with_p.order,
        ..base
    };
    let state = history.retaining_view(&forged, history.base).await;
    let reason = history.refusal(state).await;
    assert!(reason.contains("application"), "{reason}");
}

/// A new view's conflicts are its parent's plus its last application's conflict.
#[tokio::test]
async fn a_view_dropping_its_applications_conflict_is_refused() {
    let built = Built::new().await;
    let history = &built.history;
    let reader = history.reader();
    let conflicted = reader.view(built.conflicted).await.unwrap();
    let application = history.application_in(built.conflicted, built.c).await;
    let parent = reader.view(application.target_view).await.unwrap();
    assert_ne!(conflicted.unresolved_conflicts, parent.unresolved_conflicts);
    let forged = ViewRecord {
        unresolved_conflicts: parent.unresolved_conflicts,
        ..conflicted
    };
    let state = history
        .retaining_view(&forged, application.target_view)
        .await;
    let reason = history.refusal(state).await;
    assert!(reason.contains("conflicts"), "{reason}");
}

/// A new view's content map is its parent's adjusted by the tree diff.
#[tokio::test]
async fn a_view_with_its_parents_content_map_is_refused() {
    let built = Built::new().await;
    let history = &built.history;
    let reader = history.reader();
    let view = reader.view(built.with_p).await.unwrap();
    let base = reader.view(history.base).await.unwrap();
    let forged = ViewRecord {
        required_content: base.required_content,
        ..view
    };
    let state = history.retaining_view(&forged, history.base).await;
    let reason = history.refusal(state).await;
    assert!(reason.contains("content map"), "{reason}");
}

/// History only grows: each part of the published state rewound to an earlier one is
/// refused, and so is another base view.
#[tokio::test]
async fn an_increment_that_drops_history_is_refused() {
    let built = Built::new().await;
    let history = &built.history;
    let earlier = history.state_of(history.envelopes[2]).await;
    let state = history.state().await;
    let forgeries = [
        (
            RepositoryStateRecord {
                retained_views: earlier.retained_views,
                ..state
            },
            "drops retained history",
        ),
        (
            RepositoryStateRecord {
                view_lineage: earlier.view_lineage,
                ..state
            },
            "drops view lineage",
        ),
        (
            RepositoryStateRecord {
                changes: earlier.changes,
                ..state
            },
            "change map",
        ),
        (
            RepositoryStateRecord {
                required_content: earlier.required_content,
                ..state
            },
            "drops retained content",
        ),
        (
            RepositoryStateRecord {
                base_view: built.with_p,
                ..state
            },
            "base view",
        ),
    ];
    for (forged, expected) in forgeries {
        let published = history.head().envelope;
        let envelope = history.envelope(published, forged).await;
        let reason = history
            .reader()
            .validate_increment(published, envelope, IncrementBudget::STANDARD)
            .await
            .unwrap_err()
            .to_string();
        assert!(reason.contains(expected), "{expected}: {reason}");
    }
}

/// Creates change `0x90` on the base, then validates `forge(history, before, after)`
/// as an increment on the state before the creation. Returns why it is refused,
/// checking full validation refuses it too.
async fn creation_with(
    history: &mut History,
    forge: impl AsyncFnOnce(
        &History,
        RepositoryStateRecord,
        RepositoryStateRecord,
    ) -> RepositoryStateRecord,
) -> String {
    history.content.put(b"n").unwrap();
    let before = history.head().envelope;
    let base = history.base;
    history
        .create(base, change(0x90), &[], files(&[("n", None, b"n")]))
        .await;
    let (old, new) = (history.state_of(before).await, history.state().await);
    let forged = forge(history, old, new).await;
    let envelope = history.envelope(before, forged).await;
    let reader = history.reader();
    let reason = reader
        .validate_increment(before, envelope, IncrementBudget::STANDARD)
        .await
        .expect_err("the forged increment is refused")
        .to_string();
    assert!(
        reader.validate_complete(forged.id()).await.is_err(),
        "full validation accepts what the increment refuses ({reason})"
    );
    assert!(
        reader.validate_bootstrap(envelope).await.is_err(),
        "bootstrap accepts what the increment refuses ({reason})"
    );
    reason
}

/// A new application must compose a new view: the creation without its view.
#[tokio::test]
async fn an_application_composing_no_view_is_refused() {
    let mut history = History::new().await;
    let reason = creation_with(&mut history, async |_, old, new| RepositoryStateRecord {
        retained_views: old.retained_views,
        view_lineage: old.view_lineage,
        ..new
    })
    .await;
    assert!(reason.contains("composes no retained view"), "{reason}");
}

/// Each new change object has a change-map entry: the creation without its entry.
#[tokio::test]
async fn a_change_without_a_change_map_entry_is_refused() {
    let mut history = History::new().await;
    let reason = creation_with(&mut history, async |_, old, new| RepositoryStateRecord {
        changes: old.changes,
        ..new
    })
    .await;
    assert!(reason.contains("change map"), "{reason}");
}

/// The object a new change-map entry names under `key`: the creation's change object.
async fn entry_under(
    history: &History,
    old: RepositoryStateRecord,
    new: RepositoryStateRecord,
    key: ChangeId,
) -> RepositoryStateRecord {
    let object = ChangeMapStore::new(&history.objects)
        .get(new.changes, change(0x90))
        .await
        .unwrap()
        .unwrap();
    RepositoryStateRecord {
        changes: ChangeMapStore::new(&history.objects)
            .insert(old.changes, key, object)
            .await
            .unwrap(),
        ..new
    }
}

/// A change-map entry's key is its change's logical ID.
#[tokio::test]
async fn a_change_map_entry_under_another_key_is_refused() {
    let mut history = History::new().await;
    let reason = creation_with(&mut history, async |history, old, new| {
        entry_under(history, old, new, change(0x91)).await
    })
    .await;
    assert!(
        reason.contains("does not match logical change id"),
        "{reason}"
    );
}

/// An existing change keeps its object: the creation's object put under a change the
/// old state already has.
#[tokio::test]
async fn a_second_object_for_an_existing_change_is_refused() {
    let mut history = History::new().await;
    history.content.put(b"m").unwrap();
    let base = history.base;
    history
        .create(base, change(0x80), &[], files(&[("m", None, b"m")]))
        .await;
    let reason = creation_with(&mut history, async |history, old, new| {
        entry_under(history, old, new, change(0x80)).await
    })
    .await;
    assert!(reason.contains("rewrites change-map entries"), "{reason}");
}

/// Content a new revision writes is in the aggregate: the creation without its blob.
#[tokio::test]
async fn a_revision_writing_content_outside_the_aggregate_is_refused() {
    let mut history = History::new().await;
    let reason = creation_with(&mut history, async |_, old, new| RepositoryStateRecord {
        required_content: old.required_content,
        ..new
    })
    .await;
    assert!(reason.contains("aggregate"), "{reason}");
}

/// The labels for `history`'s published state: `labels` plus `main` on the base.
async fn with_labels(history: &History, labels: &[(&str, ViewId)]) -> RepositoryEnvelopeId {
    let manifest =
        ViewLabelManifestRecord::new(labels.iter().map(|(name, view)| (*name, *view))).unwrap();
    history
        .put(manifest.id().into(), &manifest.canonical_bytes())
        .await;
    let envelope = RepositoryEnvelopeRecord {
        state: history.state().await.id(),
        labels: manifest.id(),
    };
    history
        .put(envelope.id().into(), &envelope.canonical_bytes())
        .await;
    envelope.id()
}

/// A created or moved label names a view the new state retains; a kept one is not
/// looked at again.
#[tokio::test]
async fn labels_name_retained_views() {
    let built = Built::new().await;
    let history = &built.history;
    let reader = history.reader();
    let before = with_labels(history, &[("main", history.base), ("kept", history.base)]).await;
    let moved = with_labels(history, &[("main", built.with_p), ("kept", history.base)]).await;
    let report = reader
        .validate_increment(before, moved, IncrementBudget::STANDARD)
        .await
        .unwrap();
    assert_eq!(report.labels, 1);
    let unretained = ViewId::from_digest([7; 32]);
    for forged in [
        with_labels(history, &[("main", unretained), ("kept", history.base)]).await,
        with_labels(
            history,
            &[
                ("main", history.base),
                ("kept", history.base),
                ("new", unretained),
            ],
        )
        .await,
    ] {
        let reason = reader
            .validate_increment(before, forged, IncrementBudget::STANDARD)
            .await
            .unwrap_err()
            .to_string();
        assert!(reason.contains("label target not retained"), "{reason}");
    }
}

/// Integrating a change of `WIDE` edits with a small one ordered before it applies
/// the wide change's revision anew: the increment adds one application, but checking it replays every
/// one of those edits. The budget grows by them, so the increment fits the standard
/// budget; with no reads allowed per unit it is refused for its budget.
#[tokio::test]
async fn an_old_revision_replayed_anew_is_paid_for_by_its_edits() {
    const WIDE: usize = 300;
    let mut history = History::new().await;
    history.content.put(b"w").unwrap();
    history.content.put(b"s").unwrap();
    let base = history.base;
    let paths: Vec<String> = (0..WIDE).map(|index| format!("w/{index:03}")).collect();
    let writes: Vec<Write<'_>> = paths
        .iter()
        .map(|path| (path.as_str(), None, &b"w"[..]))
        .collect();
    let (wide, _) = history
        .create(base, change(0x20), &[], files(&writes))
        .await;
    let (small, _) = history
        .create(base, change(0x10), &[], files(&[("s", None, b"s")]))
        .await;
    let before = history.head().envelope;
    let (prepared, _) = history
        .mutator()
        .integrate(history.head(), base, &[wide, small])
        .await
        .unwrap();
    history.publish(prepared).await;
    let after = history.head().envelope;
    let reader = history.reader();

    let report = reader
        .validate_increment(before, after, IncrementBudget::STANDARD)
        .await
        .unwrap();
    assert!(report.budget_units > WIDE as u64, "{report:?}");
    assert!(report.budget_spent > WIDE as u64, "{report:?}");

    let refused = reader
        .validate_increment(
            before,
            after,
            IncrementBudget {
                objects_per_unit: 0,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            RepositoryPublicationError::OverBudget { allowance: 0 }
        ),
        "{refused}"
    );
}

/// The validator's future can move between threads, as the server's tasks need.
#[test]
fn validation_is_send() {
    fn send<T: Send>(_: &T) {}
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let reader = RepositoryReader::new(&objects, &content);
    let id = RepositoryEnvelopeId::from_digest([0; 32]);
    send(&reader.validate_increment(id, id, IncrementBudget::STANDARD));
}

/// Change `0xa0` writes `a`; `left` and `right` revise it adding `x` and `y`, at 100
/// and 200; `joined` combines them. `later` revises `left` adding `z`, at 300.
struct Combined {
    history: History,
    left: RevisionRecord,
    right: RevisionRecord,
    joined: RevisionRecord,
    later: RevisionRecord,
}

impl Combined {
    async fn new() -> Self {
        let mut history = History::new().await;
        for bytes in [&b"a"[..], b"x", b"y", b"z"] {
            history.content.put(bytes).unwrap();
        }
        let base = history.base;
        let c = change(0xa0);
        let (first, created) = history
            .create(base, c, &[], files(&[("a", None, b"a")]))
            .await;
        let (left_view, left) = history.add(first, c, created, "x", 100).await;
        let (right_view, right) = history.add(first, c, created, "y", 200).await;
        let (prepared, outcome) = history
            .mutator()
            .integrate(history.head(), base, &[left_view, right_view])
            .await
            .unwrap();
        history.publish(prepared).await;
        let joined = outcome.combined[&c];
        let (_, later) = history.add(left_view, c, left, "z", 300).await;
        let reader = history.reader();
        let (left, right, joined, later) = (
            reader.revision(left).await.unwrap(),
            reader.revision(right).await.unwrap(),
            reader.revision(joined).await.unwrap(),
            reader.revision(later).await.unwrap(),
        );
        Self {
            history,
            left,
            right,
            joined,
            later,
        }
    }
}

/// The transition from `predecessor`'s edits to `edits`.
fn transition(
    predecessor: &RevisionRecord,
    edits: &std::collections::BTreeSet<EditId>,
) -> EditTransition {
    EditTransition {
        preserved: predecessor.edits.intersection(edits).copied().collect(),
        added: edits.difference(&predecessor.edits).copied().collect(),
        removed: predecessor.edits.difference(edits).copied().collect(),
        ..EditTransition::default()
    }
}

/// The history the fixture built validates, combined revision included.
#[tokio::test]
async fn a_combined_revision_integration_authored_is_valid() {
    let combined = Combined::new().await;
    let history = &combined.history;
    let reader = history.reader();
    for step in history.envelopes.windows(2) {
        reader
            .validate_increment(step[0], step[1], IncrementBudget::STANDARD)
            .await
            .unwrap();
    }
    reader
        .validate_bootstrap(history.head().envelope)
        .await
        .unwrap();
    reader
        .validate_complete(history.state().await.id())
        .await
        .unwrap();
}

/// A combined revision claiming only its predecessor's edits, as if the revision it
/// combines added nothing. It replays to the predecessor's tree, so only the merge
/// check can tell.
#[tokio::test]
async fn a_combined_revision_dropping_an_inputs_edits_is_refused() {
    let combined = Combined::new().await;
    let (left, right) = (&combined.left, &combined.right);
    let forged = RevisionRecord {
        predecessor: Some(left.id()),
        combines: [right.id()].into(),
        generation: left.generation.max(right.generation) + 1,
        edits: left.edits.clone(),
        transition: transition(left, &left.edits),
        authored_against: left.authored_against,
        resulting_tree: left.resulting_tree,
        ..combined.joined.clone()
    };
    let state = combined.history.retaining_revision(&forged).await;
    let reason = combined.history.refusal(state).await;
    assert!(reason.contains("not the merge"), "{reason}");
}

/// A combined revision of `left` and its own descendant `later`. Its edits are their
/// merge, `later`'s, but a revision combining an ancestor would let a later merge read
/// that ancestor's line as removed again.
#[tokio::test]
async fn a_combined_revision_combining_its_own_ancestor_is_refused() {
    let combined = Combined::new().await;
    let (left, later) = (&combined.left, &combined.later);
    let forged = RevisionRecord {
        predecessor: Some(left.id()),
        combines: [later.id()].into(),
        generation: later.generation + 1,
        edits: later.edits.clone(),
        transition: transition(left, &later.edits),
        authored_against: later.authored_against,
        resulting_tree: later.resulting_tree,
        ..combined.joined.clone()
    };
    let state = combined.history.retaining_revision(&forged).await;
    let reason = combined.history.refusal(state).await;
    assert!(reason.contains("descend from one another"), "{reason}");
}

/// The combination of `left` and `right` recorded as following `right`, the later one.
#[tokio::test]
async fn a_combined_revision_following_a_later_input_is_refused() {
    let combined = Combined::new().await;
    let (left, right, joined) = (&combined.left, &combined.right, &combined.joined);
    assert_eq!(joined.predecessor, Some(left.id()));
    let forged = RevisionRecord {
        predecessor: Some(right.id()),
        combines: [left.id()].into(),
        transition: transition(right, &joined.edits),
        ..joined.clone()
    };
    let state = combined.history.retaining_revision(&forged).await;
    let reason = combined.history.refusal(state).await;
    assert!(reason.contains("earliest"), "{reason}");
}
