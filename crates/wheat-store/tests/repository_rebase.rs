//! Appending history from elsewhere below local work, as a Git pull does.

use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
};

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

struct Fixture {
    _dir: tempfile::TempDir,
    objects: LooseObjectStore,
    content: ContentStore,
    heads: LocalPublicationStore,
    base: ViewId,
    /// Change ids come from a counter and grow with creation order, like UUIDv7 ids, so a
    /// change created first sorts first.
    changes: AtomicUsize,
}

impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let objects =
            LooseObjectStore::with_durability(dir.path().join("objects"), Durability::Deferred);
        let content = ContentStore::new(dir.path().join("content"));
        let heads = LocalPublicationStore::new(dir.path().join("head"));
        let repository = Repository::new(Tree::new());
        let base = repository.base_view().id;
        RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap();
        Self {
            _dir: dir,
            objects,
            content,
            heads,
            base,
            changes: AtomicUsize::new(0),
        }
    }

    fn next_change(&self) -> ChangeId {
        let count = self.changes.fetch_add(1, Ordering::Relaxed) as u128 + 1;
        ChangeId::from_bytes((count << 80).to_be_bytes())
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

    /// A new change writing `bytes` to `path` on top of `effective`.
    async fn create(&self, effective: ViewId, path: &str, bytes: &[u8]) -> (ChangeId, ViewId) {
        self.content.put(bytes).unwrap();
        let change = self.next_change();
        let (prepared, outcome) = self
            .mutator()
            .create_on_frontier(
                self.head(),
                self.base,
                effective,
                ChangeProposal {
                    id: change,
                    metadata: ChangeMetadata::default(),
                    dependencies: Default::default(),
                    revision_metadata: RevisionMetadata::default(),
                    edits: exact(path, None, bytes),
                },
            )
            .await
            .unwrap();
        self.mutator().publish(prepared).await.unwrap();
        (change, outcome.resulting_view)
    }

    /// A chain of new changes, each writing one `(path, bytes)` and depending on the one
    /// before, the first on `onto`.
    fn chain(&self, onto: ChangeId, files: &[(&str, &[u8])]) -> Vec<ChangeProposal> {
        let mut previous = onto;
        files
            .iter()
            .map(|(path, bytes)| {
                self.content.put(bytes).unwrap();
                let id = self.next_change();
                let proposal = ChangeProposal {
                    id,
                    metadata: ChangeMetadata {
                        title: (*path).to_owned(),
                        ..ChangeMetadata::default()
                    },
                    dependencies: [previous].into(),
                    revision_metadata: RevisionMetadata::default(),
                    edits: exact(path, None, bytes),
                };
                previous = id;
                proposal
            })
            .collect()
    }

    async fn rebase(
        &self,
        effective: ViewId,
        onto: ChangeId,
        appended: Vec<ChangeProposal>,
    ) -> Result<RebaseOutcome, RepositoryMutationError> {
        let mutator = self.mutator();
        let session = mutator
            .open_authoring(self.head(), self.base, effective)
            .await?;
        let (prepared, outcome) = mutator
            .append_and_rebase(session, onto, appended, |old| RevisionMetadata {
                message: format!("rebased {}", old.change_id),
                ..old.metadata.clone()
            })
            .await?;
        mutator.publish(prepared).await.unwrap();
        Ok(outcome)
    }

    async fn frontiers(&self) -> Vec<ViewId> {
        let reader = self.reader();
        let state = reader
            .state(reader.envelope(self.head().envelope).await.unwrap().state)
            .await
            .unwrap();
        ViewLineageMapStore::new(&self.objects)
            .descendant_frontiers(state.view_lineage, self.base, ViewLineageLimits::default())
            .await
            .unwrap()
    }

    async fn validate(&self) {
        let reader = self.reader();
        let state = reader.envelope(self.head().envelope).await.unwrap().state;
        reader.validate_complete(state).await.unwrap();
    }

    async fn file(&self, view: ViewId, path: &str) -> Option<ContentId> {
        let tree = self.reader().view(view).await.unwrap().tree;
        MerkleTreeStore::new(&self.objects)
            .lookup(tree, &[path.to_owned()])
            .await
            .unwrap()
            .map(|value| value.content)
    }

    /// The revision `change` holds in `view` and the view it was applied onto.
    async fn applied(&self, view: ViewId, change: ChangeId) -> (RevisionRecord, ViewId) {
        let reader = self.reader();
        let view = reader.view(view).await.unwrap();
        let application = ApplicationMapStore::new(&self.objects)
            .get(view.applications, change)
            .await
            .unwrap()
            .unwrap();
        let application = reader.application(application).await.unwrap();
        (
            reader.revision(application.revision_id).await.unwrap(),
            application.target_view,
        )
    }
}

#[tokio::test]
async fn appended_changes_go_below_the_local_work() {
    let fixture = Fixture::new().await;
    let (onto, shared) = fixture.create(fixture.base, "shared", b"shared").await;
    // Created before the appended changes, so its id sorts before theirs: only its new
    // dependency can put it above them.
    let (local, effective) = fixture.create(shared, "local", b"local").await;
    let (old, _) = fixture.applied(effective, local).await;
    let old_revision = RevisionId::of_canonical_bytes(&old.canonical_bytes());
    let appended = fixture.chain(onto, &[("first", b"first"), ("second", b"second")]);
    let ids = appended.iter().map(|p| p.id).collect::<Vec<_>>();
    assert!(local < ids[0]);

    let outcome = fixture.rebase(effective, onto, appended).await.unwrap();

    assert_eq!(
        outcome
            .appended
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        ids
    );
    assert_eq!(outcome.rebased.keys().copied().collect::<Vec<_>>(), [local]);
    assert!(outcome.reapplied.is_empty());
    assert!(outcome.conflicted.is_empty());
    let result = outcome.resulting_view;
    for (path, bytes) in [
        ("shared", b"shared".as_slice()),
        ("first", b"first"),
        ("second", b"second"),
        ("local", b"local"),
    ] {
        assert_eq!(fixture.file(result, path).await, Some(ContentId::of(bytes)));
    }

    // The local change keeps its id and gets a new revision above the appended ones,
    // authored where its predecessor was.
    let (revision, target) = fixture.applied(result, local).await;
    assert_eq!(revision.change_id, local);
    assert_eq!(revision.predecessor, Some(old_revision));
    assert_eq!(revision.dependencies, BTreeSet::from([ids[1]]));
    assert_eq!(revision.authored_against, old.authored_against);
    assert_eq!(revision.resulting_tree, old.resulting_tree);
    assert_eq!(revision.metadata.message, format!("rebased {local}"));
    assert_eq!(
        fixture.file(target, "second").await,
        Some(ContentId::of(b"second"))
    );
    let (first, _) = fixture.applied(result, ids[0]).await;
    assert_eq!(first.dependencies, BTreeSet::from([onto]));

    assert_eq!(fixture.frontiers().await, [result]);
    fixture.validate().await;
}

#[tokio::test]
async fn a_stack_of_local_work_moves_above_with_its_root_revised() {
    let fixture = Fixture::new().await;
    let (onto, shared) = fixture.create(fixture.base, "shared", b"shared").await;
    let (root, middle) = fixture.create(shared, "root", b"root").await;
    let (top, effective) = fixture.create(middle, "top", b"top").await;
    let (top_before, _) = fixture.applied(effective, top).await;
    let appended = fixture.chain(onto, &[("pulled", b"pulled")]);

    let outcome = fixture.rebase(effective, onto, appended).await.unwrap();

    assert_eq!(outcome.rebased.keys().copied().collect::<Vec<_>>(), [root]);
    assert_eq!(outcome.reapplied, [top]);
    let (top_after, target) = fixture.applied(outcome.resulting_view, top).await;
    assert_eq!(top_after, top_before, "the top keeps its revision");
    assert_eq!(
        fixture.file(target, "pulled").await,
        Some(ContentId::of(b"pulled"))
    );
    assert_eq!(fixture.frontiers().await, [outcome.resulting_view]);
    fixture.validate().await;
}

#[tokio::test]
async fn local_work_that_no_longer_fits_is_a_durable_conflict() {
    let fixture = Fixture::new().await;
    let (onto, shared) = fixture.create(fixture.base, "shared", b"shared").await;
    let (local, effective) = fixture.create(shared, "same", b"local").await;
    let appended = fixture.chain(onto, &[("same", b"pulled")]);

    let outcome = fixture.rebase(effective, onto, appended).await.unwrap();

    assert_eq!(outcome.conflicted, [local]);
    let result = outcome.resulting_view;
    assert_eq!(
        fixture.file(result, "same").await,
        Some(ContentId::of(b"pulled"))
    );
    let view = fixture.reader().view(result).await.unwrap();
    let mut conflicts = ConflictSetStore::new(&fixture.objects)
        .cursor(view.unresolved_conflicts)
        .await
        .unwrap();
    assert!(conflicts.next().await.unwrap().is_some());
    assert_eq!(fixture.frontiers().await, [result]);
    fixture.validate().await;
}

#[tokio::test]
async fn appending_onto_the_newest_change_moves_nothing() {
    let fixture = Fixture::new().await;
    let (onto, effective) = fixture.create(fixture.base, "shared", b"shared").await;
    let appended = fixture.chain(onto, &[("pulled", b"pulled")]);

    let outcome = fixture.rebase(effective, onto, appended).await.unwrap();

    assert!(outcome.rebased.is_empty() && outcome.reapplied.is_empty());
    assert_eq!(fixture.frontiers().await, [outcome.resulting_view]);
    fixture.validate().await;
}

#[tokio::test]
async fn an_appended_chain_must_build_on_onto_one_change_at_a_time() {
    let fixture = Fixture::new().await;
    let (onto, shared) = fixture.create(fixture.base, "shared", b"shared").await;
    let (local, effective) = fixture.create(shared, "local", b"local").await;
    let head = fixture.head();

    let mut skipping = fixture.chain(onto, &[("a", b"a"), ("b", b"b")]);
    skipping[1].dependencies = [onto].into();
    let rejected = skipping[1].id;
    assert!(matches!(
        fixture.rebase(effective, onto, skipping).await,
        Err(RepositoryMutationError::AppendedChain(id)) if id == rejected
    ));
    let mut existing = fixture.chain(onto, &[("c", b"c")]);
    existing[0].id = local;
    assert!(matches!(
        fixture.rebase(effective, onto, existing).await,
        Err(RepositoryMutationError::Model(RepositoryError::DuplicateChange(id))) if id == local
    ));
    assert!(matches!(
        fixture.rebase(effective, onto, Vec::new()).await,
        Err(RepositoryMutationError::NothingToAppend)
    ));
    assert_eq!(fixture.head(), head, "a refused rebase publishes nothing");
}
