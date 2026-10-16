//! Full validation of revision generations, view orders and the lineage weight rule:
//! states forged from a valid one by one record each, refused for that record.

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
        self.content.put(bytes).unwrap();
        let (prepared, outcome) = self
            .mutator()
            .create(
                self.head(),
                self.base,
                self.base,
                ChangeProposal {
                    id: change,
                    metadata: ChangeMetadata::default(),
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata::default(),
                    edits: exact(path, bytes),
                },
            )
            .await
            .unwrap();
        self.mutator().publish(prepared).await.unwrap();
        (outcome.resulting_view, outcome.revision_id)
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

    /// Validates `state` in full and returns the reason it is refused, if it is.
    async fn refusal(&self, state: RepositoryStateRecord) -> Option<String> {
        self.put(state.id().into(), &state.canonical_bytes()).await;
        self.reader()
            .validate_complete(state.id())
            .await
            .err()
            .map(|error| error.to_string())
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
    let state = history.state().await;
    let view = history.reader().view(history.with_b).await.unwrap();
    let forged_view = ViewRecord {
        order: ViewOrder {
            weight: view.order.weight + 1,
            ..view.order
        },
        ..view
    };
    history
        .put(forged_view.id().into(), &forged_view.canonical_bytes())
        .await;
    let forged = RepositoryStateRecord {
        retained_views: RetainedSetStore::new(&history.objects)
            .insert(state.retained_views, forged_view.id().into())
            .await
            .unwrap(),
        view_lineage: ViewLineageMapStore::new(&history.objects)
            .insert_edges(state.view_lineage, [(history.base, forged_view.id())])
            .await
            .unwrap(),
        ..state
    };
    assert!(
        history
            .refusal(forged)
            .await
            .is_some_and(|reason| reason.contains("view order")),
    );
}
