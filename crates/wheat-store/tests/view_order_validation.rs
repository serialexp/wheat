//! Full validation of revision generations and transitions, view orders, the lineage weight rule, the
//! application order index and canonical order: states forged from a valid one by one
//! record each, refused for that record. Each is also validated as an increment on the
//! published state, which must refuse exactly what full validation refuses.

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

/// Base, then `a` and `b` each created on the base, then `a` revised.
struct History {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    base: ViewId,
    a: ChangeId,
    /// The view of `a` alone, and of `b` alone: both weigh 1.
    with_a: ViewId,
    with_b: ViewId,
    /// `a`'s first revision and its revision.
    first: RevisionId,
    revised: RevisionId,
}

impl History {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::new(dir.path().join("objects"));
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap();
        let mut history = Self {
            _dir: dir,
            objects,
            content,
            heads,
            base,
            a: ChangeId::from_bytes([1; 16]),
            with_a: base,
            with_b: base,
            first: RevisionId::from_digest([0; 32]),
            revised: RevisionId::from_digest([0; 32]),
        };
        let (with_a, first) = history.create(history.a, "a", b"a").await;
        let (with_b, _) = history
            .create(ChangeId::from_bytes([2; 16]), "b", b"b")
            .await;
        history.content.put(b"x").unwrap();
        let mut edits = exact("x", b"x");
        let original = history.reader().revision(first).await.unwrap();
        for id in &original.edits {
            edits
                .edits
                .insert(*id, history.reader().edit(*id).await.unwrap());
            edits.transition.preserved.insert(*id);
        }
        let (prepared, outcome) = history
            .mutator()
            .revise(
                history.head(),
                base,
                with_a,
                history.a,
                first,
                RevisionMetadata::default(),
                edits,
            )
            .await
            .unwrap();
        history.mutator().publish(prepared).await.unwrap();
        history.with_a = with_a;
        history.with_b = with_b;
        history.first = first;
        history.revised = outcome.revision_id;
        history
    }

    async fn create(&self, change: ChangeId, path: &str, bytes: &[u8]) -> (ViewId, RevisionId) {
        self.create_onto(self.base, change, &[], path, bytes).await
    }

    /// Creates `change`, depending on `dependencies`, with `effective` as the view to
    /// add it to.
    async fn create_onto(
        &self,
        effective: ViewId,
        change: ChangeId,
        dependencies: &[ChangeId],
        path: &str,
        bytes: &[u8],
    ) -> (ViewId, RevisionId) {
        self.content.put(bytes).unwrap();
        self.create_edits(effective, change, dependencies, exact(path, bytes))
            .await
    }

    /// Creates `change` with `edits`, their content already stored.
    async fn create_edits(
        &self,
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
        self.mutator().publish(prepared).await.unwrap();
        (outcome.resulting_view, outcome.revision_id)
    }

    /// Revises `change` from `predecessor` to `edits` alone, removing every edit the
    /// predecessor had.
    async fn replace(
        &self,
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
        self.mutator().publish(prepared).await.unwrap();
        outcome.resulting_view
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

    async fn state(&self) -> RepositoryStateRecord {
        let reader = self.reader();
        let id = reader.envelope(self.head().envelope).await.unwrap().state;
        reader.state(id).await.unwrap()
    }

    async fn put(&self, id: ObjectId, bytes: &[u8]) {
        self.objects.put_if_absent(id, bytes).await.unwrap();
    }

    /// Validates `state` in full, and as an increment on the published envelope, and
    /// returns the reason full validation refuses it, if it does. The two must agree on
    /// whether it is refused.
    async fn refusal(&self, state: RepositoryStateRecord) -> Option<String> {
        self.put(state.id().into(), &state.canonical_bytes()).await;
        let reader = self.reader();
        let full = reader
            .validate_complete(state.id())
            .await
            .err()
            .map(|error| error.to_string());
        let published = self.head().envelope;
        let envelope = RepositoryEnvelopeRecord {
            state: state.id(),
            labels: reader.envelope(published).await.unwrap().labels,
        };
        self.put(envelope.id().into(), &envelope.canonical_bytes())
            .await;
        let incremental = reader
            .validate_increment(published, envelope.id())
            .await
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            full.is_some(),
            incremental.is_some(),
            "full validation: {full:?}, incremental: {incremental:?}"
        );
        full
    }

    /// The published state with `revision` also retained.
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

    /// The published state with `application` also retained, though no view selects it.
    async fn retaining_application(
        &self,
        application: &ApplicationRecord,
    ) -> RepositoryStateRecord {
        self.put(application.id().into(), &application.canonical_bytes())
            .await;
        let state = self.state().await;
        RepositoryStateRecord {
            retained_applications: RetainedSetStore::new(&self.objects)
                .insert(state.retained_applications, application.id().into())
                .await
                .unwrap(),
            ..state
        }
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

    /// The application of `change` in `view`.
    async fn application_in(&self, view: ViewId, change: ChangeId) -> ApplicationRecord {
        let view = self.reader().view(view).await.unwrap();
        let id = ApplicationMapStore::new(&self.objects)
            .get(view.applications, change)
            .await
            .unwrap()
            .unwrap();
        self.reader().application(id).await.unwrap()
    }
}

/// `y` and `z` on the base, then `k` depending on `z`: `k` has the lowest ID but is
/// last in canonical order, after its dependency.
struct Chain {
    history: History,
    y: ChangeId,
    z: ChangeId,
    k: ChangeId,
    with_y: ViewId,
    with_yzk: ViewId,
}

impl Chain {
    async fn new() -> Self {
        let history = History::new().await;
        let (y, z, k) = (
            ChangeId::from_bytes([0x20; 16]),
            ChangeId::from_bytes([0x30; 16]),
            ChangeId::from_bytes([0x10; 16]),
        );
        let (with_y, _) = history.create(y, "y", b"y").await;
        let (with_yz, _) = history.create_onto(with_y, z, &[], "z", b"z").await;
        let (with_yzk, _) = history.create_onto(with_yz, k, &[z], "k", b"k").await;
        Self {
            history,
            y,
            z,
            k,
            with_y,
            with_yzk,
        }
    }
}

#[tokio::test]
async fn the_published_history_validates() {
    let history = History::new().await;
    assert_eq!(history.refusal(history.state().await).await, None);
    let reader = history.reader();
    assert_eq!(reader.revision(history.first).await.unwrap().generation, 0);
    assert_eq!(
        reader.revision(history.revised).await.unwrap().generation,
        1
    );
    for view in [history.with_a, history.with_b] {
        let order = reader.view(view).await.unwrap().order;
        assert_eq!(order.size, 1);
        assert_eq!(order.weight, 1);
    }
}

/// A revision's generation is one more than its parents'.
#[tokio::test]
async fn a_revision_with_the_wrong_generation_is_refused() {
    let history = History::new().await;
    let revised = history.reader().revision(history.revised).await.unwrap();
    // The same revision under another message, so it is another object, is fine at
    // generation 1 and refused at any other.
    let copy = |generation| RevisionRecord {
        metadata: RevisionMetadata {
            message: "copy".into(),
            ..revised.metadata.clone()
        },
        generation,
        ..revised.clone()
    };
    let control = history.retaining_revision(&copy(1)).await;
    assert_eq!(history.refusal(control).await, None);
    let forged = history.retaining_revision(&copy(2)).await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("generation")),
    );
}

/// A revision's transition partitions its predecessor's edits exactly: each is
/// preserved or removed.
#[tokio::test]
async fn a_transition_that_loses_a_predecessor_edit_is_refused() {
    let history = History::new().await;
    let revised = history.reader().revision(history.revised).await.unwrap();
    let first = history.reader().revision(history.first).await.unwrap();
    assert!(!first.edits.is_empty());
    assert_eq!(revised.transition.preserved, first.edits);
    // The same revision under another message, so it is another object, validates.
    let copy = |transition| RevisionRecord {
        metadata: RevisionMetadata {
            message: "copy".into(),
            ..revised.metadata.clone()
        },
        transition,
        ..revised.clone()
    };
    let control = history
        .retaining_revision(&copy(revised.transition.clone()))
        .await;
    assert_eq!(history.refusal(control).await, None);
    // Every edit called added, the predecessor's too: the edits and the replay are
    // the same, but the predecessor's edit is neither preserved nor removed.
    let forged = history
        .retaining_revision(&copy(EditTransition {
            added: revised.edits.clone(),
            ..EditTransition::default()
        }))
        .await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("partition")),
    );
}

/// `p` writes `f` and `g`, and `c` on top rewrites both. Revising `p` makes `c`
/// conflict on both edits; `c` is resolved, and revising `p` again makes the resolved
/// revision conflict too.
struct Resolved {
    history: History,
    /// The revision that resolves `c`'s first conflict.
    resolving: RevisionId,
    /// The application of `resolving` that conflicts after `p`'s second revision.
    conflicted_again: ApplicationRecord,
}

impl Resolved {
    async fn new() -> Self {
        let history = History::new().await;
        for bytes in [
            b"f0", b"g0", b"cf", b"cg", b"f1", b"g1", b"f2", b"g2", b"rf", b"rg",
        ] {
            history.content.put(bytes).unwrap();
        }
        let (p, c) = (
            ChangeId::from_bytes([0x40; 16]),
            ChangeId::from_bytes([0x50; 16]),
        );
        let (with_p, p1) = history
            .create_edits(
                history.base,
                p,
                &[],
                files(&[("f", None, b"f0"), ("g", None, b"g0")]),
            )
            .await;
        let (with_c, _) = history
            .create_edits(
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
                history.base,
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
        history.mutator().publish(prepared).await.unwrap();
        let p2 = history.application_in(resolved.resulting_view, p).await;
        let again = history
            .replace(
                resolved.resulting_view,
                p,
                p2.revision_id,
                files(&[("f", None, b"f2"), ("g", None, b"g2")]),
            )
            .await;
        let conflicted_again = history.application_in(again, c).await;
        assert_eq!(conflicted_again.revision_id, resolved.revision_id);
        assert!(matches!(
            conflicted_again.result,
            ApplicationResultRecord::Conflicted { .. }
        ));
        assert_eq!(history.refusal(history.state().await).await, None);
        Self {
            history,
            resolving: resolved.revision_id,
            conflicted_again,
        }
    }

    /// The resolving revision, under another message so it is another object, with
    /// its resolution changed by `change`.
    async fn forged(&self, change: impl FnOnce(&mut ConflictResolutionEvidence)) -> RevisionRecord {
        let revision = self
            .history
            .reader()
            .revision(self.resolving)
            .await
            .unwrap();
        let mut transition = revision.transition.clone();
        change(&mut transition.resolutions[0]);
        RevisionRecord {
            metadata: RevisionMetadata {
                message: "forged".into(),
                ..revision.metadata.clone()
            },
            transition,
            ..revision
        }
    }
}

/// A resolution may name some of its conflict's failed edits, not only all of them.
#[tokio::test]
async fn a_resolution_of_some_failed_edits_validates() {
    let resolved = Resolved::new().await;
    let partial = resolved
        .forged(|evidence| {
            assert_eq!(evidence.conflicted_edits.len(), 2);
            evidence.conflicted_edits.pop_first();
        })
        .await;
    let state = resolved.history.retaining_revision(&partial).await;
    assert_eq!(resolved.history.refusal(state).await, None);
    // An edit the conflict doesn't have is refused.
    let foreign = resolved
        .forged(|evidence| {
            evidence.conflicted_edits = [EditId::from_digest([7; 32])].into();
        })
        .await;
    let state = resolved.history.retaining_revision(&foreign).await;
    assert!(
        resolved
            .history
            .refusal(state)
            .await
            .is_some_and(|reason| reason.contains("resolution conflict evidence")),
    );
}

/// A revision resolves a conflict of its predecessor's application: naming a
/// conflicted application of the revision itself is refused.
#[tokio::test]
async fn a_resolution_of_another_revisions_application_is_refused() {
    let resolved = Resolved::new().await;
    let ApplicationResultRecord::Conflicted { conflict, .. } = resolved.conflicted_again.result
    else {
        unreachable!("checked in Resolved::new");
    };
    let failed = resolved
        .history
        .reader()
        .conflict(conflict)
        .await
        .unwrap()
        .edits
        .keys()
        .copied()
        .collect();
    let forged = resolved
        .forged(|evidence| {
            evidence.application = resolved.conflicted_again.id();
            evidence.conflicted_edits = failed;
        })
        .await;
    let state = resolved.history.retaining_revision(&forged).await;
    assert!(
        resolved
            .history
            .refusal(state)
            .await
            .is_some_and(|reason| reason.contains("predecessor's")),
    );
}

/// A combined revision combines revisions of its own change.
#[tokio::test]
async fn a_revision_combining_another_changes_revision_is_refused() {
    let history = History::new().await;
    let reader = history.reader();
    let revised = reader.revision(history.revised).await.unwrap();
    let other_change = reader
        .revision(
            RevisionMapStore::new(&history.objects)
                .get(
                    reader.view(history.with_b).await.unwrap().revisions,
                    ChangeId::from_bytes([2; 16]),
                )
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
    let forged = RevisionRecord {
        combines: [other_change.id()].into(),
        ..revised
    };
    let state = history.retaining_revision(&forged).await;
    assert!(
        history
            .refusal(state)
            .await
            .is_some_and(|reason| reason.contains("another change")),
    );
}

/// An edge between two views of equal weight is no cycle, but the weight rule refuses
/// it: unioned with a state holding the reverse edge, it would be one.
#[tokio::test]
async fn a_lineage_edge_that_does_not_increase_the_weight_is_refused() {
    let history = History::new().await;
    let state = history.state().await;
    let forged = RepositoryStateRecord {
        view_lineage: ViewLineageMapStore::new(&history.objects)
            .insert_edges(state.view_lineage, [(history.with_a, history.with_b)])
            .await
            .unwrap(),
        ..state
    };
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("weight")),
    );
}

/// A view's order follows its composition parent's.
#[tokio::test]
async fn a_view_with_the_wrong_weight_is_refused() {
    let history = History::new().await;
    let view = history.reader().view(history.with_b).await.unwrap();
    let forged_view = ViewRecord {
        order: ViewOrder {
            weight: view.order.weight + 1,
            ..view.order
        },
        ..view
    };
    let forged = history.retaining_view(&forged_view, history.base).await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("view order")),
    );
}

/// A view's maps are its composition parent's plus its last change and nothing else
/// (I1): `a`'s view also selecting `b`, with `a`'s order, tree and edge, is refused.
#[tokio::test]
async fn a_view_selecting_more_than_its_parent_and_last_change_is_refused() {
    let history = History::new().await;
    let b = ChangeId::from_bytes([2; 16]);
    let with_b = history.reader().view(history.with_b).await.unwrap();
    let b_revision = RevisionMapStore::new(&history.objects)
        .get(with_b.revisions, b)
        .await
        .unwrap()
        .unwrap();
    let b_application = history.application_in(history.with_b, b).await.id();
    let view = history.reader().view(history.with_a).await.unwrap();
    let forged_view = ViewRecord {
        revisions: RevisionMapStore::new(&history.objects)
            .insert(view.revisions, b, b_revision)
            .await
            .unwrap(),
        applications: ApplicationMapStore::new(&history.objects)
            .insert(view.applications, b, b_application)
            .await
            .unwrap(),
        ..view
    };
    let forged = history.retaining_view(&forged_view, history.base).await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("composition parent's plus one change")),
    );
}

/// A view's tree is its last application's result (I1).
#[tokio::test]
async fn a_view_whose_tree_is_not_its_last_applications_result_is_refused() {
    let history = History::new().await;
    let with_b = history.reader().view(history.with_b).await.unwrap();
    let view = history.reader().view(history.with_a).await.unwrap();
    let forged_view = ViewRecord {
        tree: with_b.tree,
        required_content: with_b.required_content,
        ..view
    };
    let forged = history.retaining_view(&forged_view, history.base).await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("result of its last application")),
    );
}

/// A view has a lineage edge from its composition parent (I1). `k`'s view with
/// another tree, reached only from `y`'s view two changes below, is refused for the
/// missing edge before anything else about it.
#[tokio::test]
async fn a_view_without_an_edge_from_its_composition_parent_is_refused() {
    let chain = Chain::new().await;
    let history = &chain.history;
    let view = history.reader().view(chain.with_yzk).await.unwrap();
    let with_y = history.reader().view(chain.with_y).await.unwrap();
    let forged_view = ViewRecord {
        tree: with_y.tree,
        required_content: with_y.required_content,
        ..view
    };
    let forged = history.retaining_view(&forged_view, chain.with_y).await;
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("lineage edge from its composition parent")),
    );
}

/// A change with a lower ID than one before it is last when it depends on that one.
#[tokio::test]
async fn a_change_after_its_dependency_validates_whatever_its_id() {
    let chain = Chain::new().await;
    let history = &chain.history;
    assert_eq!(history.refusal(history.state().await).await, None);
    let k = history.application_in(chain.with_yzk, chain.k).await;
    let y = history.application_in(chain.with_yzk, chain.y).await;
    let z = history.application_in(chain.with_yzk, chain.z).await;
    assert_eq!(
        (y.order.position, z.order.position, k.order.position),
        (0, 1, 2)
    );
    assert_eq!(k.order.previous, Some(z.id()));
    assert_eq!(k.order.skip, [z.id(), y.id()]);
    assert_eq!(k.order.window_max, [chain.k, chain.z]);
}

/// Each field of the order index is recomputed from the target view.
#[tokio::test]
async fn an_application_with_a_wrong_order_index_is_refused() {
    let chain = Chain::new().await;
    let history = &chain.history;
    let k = history.application_in(chain.with_yzk, chain.k).await;
    let y = history.application_in(chain.with_yzk, chain.y).await;
    let z = history.application_in(chain.with_yzk, chain.z).await;
    let forged_orders = [
        ApplicationOrder {
            previous: Some(y.id()),
            skip: vec![y.id(), y.id()],
            ..k.order.clone()
        },
        ApplicationOrder {
            skip: vec![z.id(), z.id()],
            ..k.order.clone()
        },
        ApplicationOrder {
            window_max: vec![chain.k, ChangeId::from_bytes([0x40; 16])],
            ..k.order.clone()
        },
        ApplicationOrder {
            window_max: vec![chain.y, chain.z],
            ..k.order.clone()
        },
    ];
    for order in forged_orders {
        let forged = ApplicationRecord {
            order: order.clone(),
            ..k.clone()
        };
        let state = history.retaining_application(&forged).await;
        let reason = history.refusal(state).await;
        assert!(
            reason
                .as_ref()
                .is_some_and(|reason| reason.contains("order index")),
            "{order:?}: {reason:?}"
        );
    }
    // An index that does not fit its position is not even stored.
    let forged = ApplicationRecord {
        order: ApplicationOrder {
            position: 3,
            ..k.order.clone()
        },
        ..k
    };
    let error = history
        .objects
        .put_if_absent(forged.id().into(), &forged.canonical_bytes())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("does not fit its position"),
        "{error}"
    );
}

/// `change` created with `with_y` as the effective view, then forged as applied onto
/// `with_y` itself: the view the mutator makes when `change` comes after `y` in
/// canonical order. Returns the state retaining the forged view, its ID, and the view
/// the mutator made.
async fn applied_after_y(
    chain: &Chain,
    change: ChangeId,
    path: &str,
) -> (RepositoryStateRecord, ViewId, ViewId) {
    let history = &chain.history;
    let (with_both, revision) = history
        .create_onto(chain.with_y, change, &[], path, path.as_bytes())
        .await;
    let reader = history.reader();
    let target = reader.view(chain.with_y).await.unwrap();
    let both = reader.view(with_both).await.unwrap();
    let previous = history.application_in(chain.with_y, chain.y).await.id();
    let mut order = ApplicationOrder::begin(change, target.order.size, Some(previous));
    while let Some(id) = order.wanted() {
        order
            .extend(&reader.application(id).await.unwrap().order)
            .unwrap();
    }
    let application = ApplicationRecord {
        revision_id: revision,
        target_view: chain.with_y,
        order,
        result: ApplicationResultRecord::Clean { tree: both.tree },
        applied: history.application_in(with_both, change).await.applied,
    };
    history
        .put(application.id().into(), &application.canonical_bytes())
        .await;
    let view = ViewRecord {
        tree: both.tree,
        revisions: RevisionMapStore::new(&history.objects)
            .insert(target.revisions, change, revision)
            .await
            .unwrap(),
        applications: ApplicationMapStore::new(&history.objects)
            .insert(target.applications, change, application.id())
            .await
            .unwrap(),
        unresolved_conflicts: target.unresolved_conflicts,
        required_content: both.required_content,
        order: target.order.then(change, 0).unwrap(),
    };
    history.put(view.id().into(), &view.canonical_bytes()).await;
    let state = history.state().await;
    let retained = RetainedSetStore::new(&history.objects);
    let state = RepositoryStateRecord {
        retained_views: retained
            .insert(state.retained_views, view.id().into())
            .await
            .unwrap(),
        retained_applications: retained
            .insert(state.retained_applications, application.id().into())
            .await
            .unwrap(),
        view_lineage: ViewLineageMapStore::new(&history.objects)
            .insert_edges(state.view_lineage, [(chain.with_y, view.id())])
            .await
            .unwrap(),
        ..state
    };
    (state, view.id(), with_both)
}

/// The forgery above is exact: for a change after `y` it is the view the mutator made.
#[tokio::test]
async fn a_view_built_in_canonical_order_is_the_one_the_mutator_makes() {
    let chain = Chain::new().await;
    let (state, forged, made) =
        applied_after_y(&chain, ChangeId::from_bytes([0x28; 16]), "w").await;
    assert_eq!(forged, made);
    assert_eq!(chain.history.refusal(state).await, None);
}

/// With no dependencies, a lower ID comes first: `x` applied after `y` is refused.
#[tokio::test]
async fn a_view_out_of_canonical_order_is_refused() {
    let chain = Chain::new().await;
    let (state, forged, made) =
        applied_after_y(&chain, ChangeId::from_bytes([0x08; 16]), "x").await;
    assert_ne!(forged, made);
    let reason = chain.history.refusal(state).await;
    assert!(
        reason
            .as_ref()
            .is_some_and(|reason| reason.contains("canonical order")),
        "{reason:?}"
    );
}
