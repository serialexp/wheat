//! History-based delta bases for packed metadata and content.
//!
//! Repack stores an older object as a delta against its newer counterpart
//! (a reverse delta). This module only proposes bases; identity never
//! changes, and the writers still keep a delta only when it is smaller than
//! the whole record (and, for metadata, when its base shares the segment).
//!
//! Planning reads the stored repository state directly: the retained-view set
//! and the view-lineage map give the views and their edges. It never builds a
//! whole in-memory repository, so its memory follows what changed between
//! views rather than the size of every view.
//!
//! For each view-lineage edge whose child has exactly one parent, the older
//! (parent) view's objects are paired with the newer (child) view's objects at
//! the same position:
//!
//! - directory nodes at the same path, descending only where IDs differ;
//! - file content at the same path, where it differs;
//! - revision- and application-map radix nodes at the same edge path;
//! - the views' required-content sets.
//!
//! A target claimed by two different bases, or proposed as its own base, stays
//! whole. Every object and file reachable from a tip view (a view without
//! children) stays whole, so the newest state never pays for delta
//! resolution.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::{
    CanonicalObjectError, ContentId, DirectoryEntry, ObjectId, RepositoryStateId, ViewId,
    decode_application_map_node, decode_content_map_node, decode_directory_node,
    decode_repository_state_record, decode_revision_map_node, decode_view_record,
};

use crate::{
    CollectionError, ContentHistoryPair, ImmutableObjectStore, ObjectStoreError, RetainedSetStore,
    ViewLineageError, ViewLineageMapStore,
};

type Digest = [u8; 32];

#[derive(Debug, Error)]
pub(crate) enum HistoryPlanError {
    #[error(transparent)]
    Store(#[from] ObjectStoreError),
    #[error(transparent)]
    Canonical(#[from] CanonicalObjectError),
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    Lineage(#[from] ViewLineageError),
    #[error("history planning exceeds its {limit} {kind} limit")]
    Limit { kind: &'static str, limit: usize },
}

/// Bounds on what one planning pass may collect. Exceeding one is an error
/// rather than a silently partial plan.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HistoryPlanLimits {
    pub metadata_proposals: usize,
    pub content_pairs: usize,
    pub content_tips: usize,
}

/// Counters describing one planning pass, for reports and diagnostics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct MetadataDeltaStats {
    pub views: u64,
    pub single_parent_edges: u64,
    pub proposals: u64,
    pub conflicts: u64,
    pub tip_claims: u64,
}

/// Proposed physical delta bases: older target object -> newer base object.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct MetadataDeltaPlan {
    pub bases: BTreeMap<Digest, Digest>,
    pub stats: MetadataDeltaStats,
}

/// Retained content history, the content repack planner's input.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ContentHistory {
    /// Content of every file in a tip view.
    pub tips: BTreeSet<ContentId>,
    /// Differing file content at the same path across single-parent edges.
    pub pairs: BTreeSet<ContentHistoryPair>,
}

/// Everything repack plans from retained history, in one pass.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct HistoryPlan {
    pub metadata: MetadataDeltaPlan,
    pub content: ContentHistory,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Proposal {
    Unique(Digest),
    Conflict,
}

#[derive(Clone, Copy)]
struct ViewRoots {
    tree: Digest,
    revisions: Digest,
    applications: Digest,
    content: Digest,
}

#[derive(Clone, Copy)]
enum MapKind {
    Revision,
    Application,
    Content,
}

struct Proposals {
    bases: BTreeMap<Digest, Proposal>,
    limit: usize,
}

impl Proposals {
    fn propose(&mut self, target: Digest, base: Digest) -> Result<(), HistoryPlanError> {
        let proposed = if target == base {
            Proposal::Conflict
        } else {
            Proposal::Unique(base)
        };
        if let Some(choice) = self.bases.get_mut(&target) {
            if *choice != proposed {
                *choice = Proposal::Conflict;
            }
            return Ok(());
        }
        if self.bases.len() == self.limit {
            return Err(HistoryPlanError::Limit {
                kind: "metadata proposal",
                limit: self.limit,
            });
        }
        self.bases.insert(target, proposed);
        Ok(())
    }
}

/// Collects [`ContentHistory`] within its limits.
struct ContentCollector {
    history: ContentHistory,
    limits: HistoryPlanLimits,
}

impl ContentCollector {
    fn pair(&mut self, older: ContentId, newer: ContentId) -> Result<(), HistoryPlanError> {
        if self
            .history
            .pairs
            .insert(ContentHistoryPair { older, newer })
            && self.history.pairs.len() > self.limits.content_pairs
        {
            return Err(HistoryPlanError::Limit {
                kind: "content history pair",
                limit: self.limits.content_pairs,
            });
        }
        Ok(())
    }

    fn tip(&mut self, id: ContentId) -> Result<(), HistoryPlanError> {
        if self.history.tips.insert(id) && self.history.tips.len() > self.limits.content_tips {
            return Err(HistoryPlanError::Limit {
                kind: "content tip",
                limit: self.limits.content_tips,
            });
        }
        Ok(())
    }
}

/// The retained views of a stored state and its view-lineage edges
/// `(parent, child)`, read without hydrating any view.
async fn stored_lineage<S: ImmutableObjectStore>(
    store: &S,
    state: RepositoryStateId,
) -> Result<(BTreeSet<ViewId>, Vec<(ViewId, ViewId)>), HistoryPlanError> {
    let state = decode_repository_state_record(&read(store, *state.as_bytes()).await?)?;
    let retained = RetainedSetStore::new(store);
    let mut views = BTreeSet::new();
    let mut cursor = retained.cursor(state.retained_views).await?;
    while let Some(id) = cursor.next().await? {
        views.insert(ViewId::from_digest(*id.as_bytes()));
    }
    let lineage = ViewLineageMapStore::new(store);
    let mut edges = Vec::new();
    for parent in &views {
        let Some(children) = lineage.get(state.view_lineage, *parent).await? else {
            continue;
        };
        let mut children = retained.cursor(children).await?;
        while let Some(child) = children.next().await? {
            edges.push((*parent, ViewId::from_digest(*child.as_bytes())));
        }
    }
    Ok((views, edges))
}

/// Plans history-based metadata bases and collects content history for every
/// retained view-lineage edge of the stored `state`, which the caller has
/// already validated.
pub(crate) async fn plan_history_deltas<S: ImmutableObjectStore>(
    store: &S,
    state: RepositoryStateId,
    limits: HistoryPlanLimits,
) -> Result<HistoryPlan, HistoryPlanError> {
    let (mut view_ids, edges) = stored_lineage(store, state).await?;
    let mut parents = BTreeMap::<ViewId, Vec<ViewId>>::new();
    let mut has_children = BTreeSet::new();
    for (parent, child) in edges {
        view_ids.insert(parent);
        view_ids.insert(child);
        parents.entry(child).or_default().push(parent);
        has_children.insert(parent);
    }
    let mut views = BTreeMap::new();
    for id in &view_ids {
        views.insert(*id, view_roots(store, *id).await?);
    }

    let mut proposals = Proposals {
        bases: BTreeMap::new(),
        limit: limits.metadata_proposals,
    };
    let mut content = ContentCollector {
        history: ContentHistory::default(),
        limits,
    };
    let mut stats = MetadataDeltaStats {
        views: view_ids.len() as u64,
        ..MetadataDeltaStats::default()
    };
    for (child, child_parents) in &parents {
        let [parent] = child_parents.as_slice() else {
            continue;
        };
        stats.single_parent_edges += 1;
        let (older, newer) = (views[parent], views[child]);
        pair_trees(store, &mut proposals, &mut content, older.tree, newer.tree).await?;
        pair_maps(
            store,
            &mut proposals,
            MapKind::Revision,
            older.revisions,
            newer.revisions,
        )
        .await?;
        pair_maps(
            store,
            &mut proposals,
            MapKind::Application,
            older.applications,
            newer.applications,
        )
        .await?;
        pair_maps(
            store,
            &mut proposals,
            MapKind::Content,
            older.content,
            newer.content,
        )
        .await?;
    }
    stats.proposals = proposals.bases.len() as u64;
    stats.conflicts = proposals
        .bases
        .values()
        .filter(|choice| **choice == Proposal::Conflict)
        .count() as u64;

    let mut tips = BTreeSet::new();
    for (id, roots) in &views {
        if !has_children.contains(id) {
            reachable_from_view(store, *roots, &mut tips, &mut content).await?;
        }
    }
    for tip in &tips {
        if proposals.bases.remove(tip).is_some() {
            stats.tip_claims += 1;
        }
    }
    let bases = proposals
        .bases
        .into_iter()
        .filter_map(|(target, choice)| match choice {
            Proposal::Unique(base) => Some((target, base)),
            Proposal::Conflict => None,
        })
        .collect();
    Ok(HistoryPlan {
        metadata: MetadataDeltaPlan { bases, stats },
        content: content.history,
    })
}

async fn read<S: ImmutableObjectStore>(
    store: &S,
    id: Digest,
) -> Result<bytes::Bytes, ObjectStoreError> {
    Ok(store.get(ObjectId::from_digest(id)).await?.canonical_bytes)
}

async fn view_roots<S: ImmutableObjectStore>(
    store: &S,
    id: ViewId,
) -> Result<ViewRoots, HistoryPlanError> {
    let view = decode_view_record(&read(store, *id.as_bytes()).await?)?;
    Ok(ViewRoots {
        tree: *view.tree.as_bytes(),
        revisions: *view.revisions.as_bytes(),
        applications: *view.applications.as_bytes(),
        content: *view.required_content.as_bytes(),
    })
}

async fn directory_entries<S: ImmutableObjectStore>(
    store: &S,
    id: Digest,
) -> Result<BTreeMap<String, DirectoryEntry>, HistoryPlanError> {
    Ok(decode_directory_node(&read(store, id).await?)?.entries)
}

async fn map_children<S: ImmutableObjectStore>(
    store: &S,
    kind: MapKind,
    id: Digest,
) -> Result<BTreeMap<u8, Digest>, HistoryPlanError> {
    let bytes = read(store, id).await?;
    Ok(match kind {
        MapKind::Revision => decode_revision_map_node(&bytes)?
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
        MapKind::Application => decode_application_map_node(&bytes)?
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
        MapKind::Content => decode_content_map_node(&bytes)?
            .children
            .into_iter()
            .map(|child| (child.edge, *child.child.as_bytes()))
            .collect(),
    })
}

/// Pairs the older and newer directory, and the older and newer file content,
/// at every shared path whose IDs differ. Identical subtrees are skipped
/// without being read.
async fn pair_trees<S: ImmutableObjectStore>(
    store: &S,
    proposals: &mut Proposals,
    content: &mut ContentCollector,
    older: Digest,
    newer: Digest,
) -> Result<(), HistoryPlanError> {
    let mut pending = vec![(older, newer)];
    while let Some((older, newer)) = pending.pop() {
        if older == newer {
            continue;
        }
        proposals.propose(older, newer)?;
        let newer_entries = directory_entries(store, newer).await?;
        for (name, older_entry) in directory_entries(store, older).await? {
            match (older_entry, newer_entries.get(&name)) {
                (DirectoryEntry::Directory(older), Some(DirectoryEntry::Directory(newer))) => {
                    pending.push((*older.as_bytes(), *newer.as_bytes()));
                }
                (DirectoryEntry::File(older), Some(DirectoryEntry::File(newer)))
                    if older.content != newer.content =>
                {
                    content.pair(older.content, newer.content)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Pairs the older and newer radix node at every shared edge path whose IDs
/// differ.
async fn pair_maps<S: ImmutableObjectStore>(
    store: &S,
    proposals: &mut Proposals,
    kind: MapKind,
    older: Digest,
    newer: Digest,
) -> Result<(), HistoryPlanError> {
    let mut pending = vec![(older, newer)];
    while let Some((older, newer)) = pending.pop() {
        if older == newer {
            continue;
        }
        proposals.propose(older, newer)?;
        let newer_children = map_children(store, kind, newer).await?;
        for (edge, older_child) in map_children(store, kind, older).await? {
            if let Some(newer_child) = newer_children.get(&edge) {
                pending.push((older_child, *newer_child));
            }
        }
    }
    Ok(())
}

/// Every pairable object reachable from a view (its directories and its three
/// maps' radix nodes) into `into`, and its files' content into
/// `content`'s tips. A directory already in `into` was walked by an earlier
/// view, so shared subtrees are read once.
async fn reachable_from_view<S: ImmutableObjectStore>(
    store: &S,
    view: ViewRoots,
    into: &mut BTreeSet<Digest>,
    content: &mut ContentCollector,
) -> Result<(), HistoryPlanError> {
    let mut trees = vec![view.tree];
    while let Some(tree) = trees.pop() {
        if !into.insert(tree) {
            continue;
        }
        for entry in directory_entries(store, tree).await?.into_values() {
            match entry {
                DirectoryEntry::Directory(child) => trees.push(*child.as_bytes()),
                DirectoryEntry::File(file) => content.tip(file.content)?,
            }
        }
    }
    for (kind, root) in [
        (MapKind::Revision, view.revisions),
        (MapKind::Application, view.applications),
        (MapKind::Content, view.content),
    ] {
        let mut nodes = vec![root];
        while let Some(node) = nodes.pop() {
            if into.insert(node) {
                nodes.extend(map_children(store, kind, node).await?.into_values());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use wheat_core::{
        ApplicationContext, ChangeId, EditOperation, EditRecord, EditTransition, FileKind,
        FileValue, Repository, RevisionEdits, Tree,
    };

    use super::*;
    use crate::{ContentStore, LooseObjectStore};

    const WIDTH: usize = 16;

    fn file(bytes: &[u8]) -> FileValue {
        FileValue {
            content: ContentId::of(bytes),
            kind: FileKind::Regular,
        }
    }

    fn replace(path: &str, expected: &[u8], value: &[u8]) -> RevisionEdits {
        let record = EditRecord::new(
            path,
            EditOperation::ExactFile {
                expected: Some(file(expected)),
                replacement: Some(file(value)),
            },
        )
        .unwrap();
        let id = record.id();
        RevisionEdits {
            edits: [(id, record)].into(),
            transition: EditTransition {
                added: [id].into(),
                ..EditTransition::default()
            },
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        objects: LooseObjectStore,
        content: ContentStore,
        repo: Repository,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let objects = LooseObjectStore::new(directory.path().join("objects"));
            let content = ContentStore::new(directory.path().join("content"));
            content.put(b"initial").unwrap();
            let tree = (0..WIDTH)
                .map(|index| (format!("dir/file{index:02}"), file(b"initial")))
                .collect::<Tree>();
            Self {
                _directory: directory,
                objects,
                content,
                repo: Repository::new(tree),
            }
        }

        /// Change `id` replaces `dir/file{index}` (currently `expected`) on
        /// top of `effective`, depending on `previous` when it edited the same
        /// file.
        fn change(
            &mut self,
            effective: ViewId,
            id: ChangeId,
            previous: Option<ChangeId>,
            index: usize,
            expected: &[u8],
            value: &[u8],
        ) -> ViewId {
            self.content.put(value).unwrap();
            let basis = self.repo.base_view().id;
            self.repo
                .create_change_with_id(
                    basis,
                    effective,
                    id,
                    previous,
                    replace(&format!("dir/file{index:02}"), expected, value),
                    ApplicationContext::new(&self.content),
                )
                .unwrap()
                .resulting_view
        }

        /// Replaces the still-initial `dir/file{index}` on top of `effective`.
        fn edit(&mut self, effective: ViewId, index: usize, value: &[u8]) -> ViewId {
            self.change(effective, ChangeId::new(), None, index, b"initial", value)
        }

        /// Stores the repository's canonical closure and returns its state.
        async fn store(&self) -> RepositoryStateId {
            let closure = self.repo.canonical_closure();
            for object in closure.objects {
                self.objects
                    .put_if_absent(object.id, &object.bytes)
                    .await
                    .unwrap();
            }
            closure.state.id
        }

        async fn plan_with(
            &self,
            limits: HistoryPlanLimits,
        ) -> Result<HistoryPlan, HistoryPlanError> {
            let state = self.store().await;
            plan_history_deltas(&self.objects, state, limits).await
        }

        async fn plan(&self) -> HistoryPlan {
            self.plan_with(LIMITS).await.unwrap()
        }

        async fn roots(&self, view: ViewId) -> ViewRoots {
            view_roots(&self.objects, view).await.unwrap()
        }

        /// Everything a tip walk over `views` keeps whole.
        async fn tips(&self, views: &[ViewId]) -> (BTreeSet<Digest>, ContentHistory) {
            let mut objects = BTreeSet::new();
            let mut content = ContentCollector {
                history: ContentHistory::default(),
                limits: LIMITS,
            };
            for view in views {
                let roots = self.roots(*view).await;
                reachable_from_view(&self.objects, roots, &mut objects, &mut content)
                    .await
                    .unwrap();
            }
            (objects, content.history)
        }
    }

    const LIMITS: HistoryPlanLimits = HistoryPlanLimits {
        metadata_proposals: 10_000,
        content_pairs: 10_000,
        content_tips: 10_000,
    };

    fn pair(older: &[u8], newer: &[u8]) -> ContentHistoryPair {
        ContentHistoryPair {
            older: ContentId::of(older),
            newer: ContentId::of(newer),
        }
    }

    #[tokio::test]
    async fn linear_history_pairs_each_older_view_with_its_successor() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        let first = fixture.edit(base, 0, b"one");
        let second_view = fixture.edit(first, 1, b"two");
        let history = fixture.plan().await;
        let plan = &history.metadata;
        let [base, first, second] = [
            fixture.roots(base).await,
            fixture.roots(first).await,
            fixture.roots(second_view).await,
        ];

        assert_eq!(plan.stats.views, 3);
        assert_eq!(plan.stats.single_parent_edges, 2);
        assert_eq!(plan.stats.conflicts, 0);
        // Older roots delta against the next view's roots.
        assert_eq!(plan.bases.get(&base.tree), Some(&first.tree));
        assert_eq!(plan.bases.get(&first.tree), Some(&second.tree));
        assert_eq!(plan.bases.get(&base.content), Some(&first.content));
        assert_eq!(plan.bases.get(&first.content), Some(&second.content));
        // The tip view stays whole.
        let (tip, _) = fixture.tips(&[second_view]).await;
        assert!(plan.bases.keys().all(|target| !tip.contains(target)));
        assert!(plan.bases.iter().all(|(target, base)| target != base));
    }

    #[tokio::test]
    async fn siblings_claiming_one_parent_leave_it_whole() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        let left = fixture.edit(base, 0, b"left");
        let right = fixture.edit(base, 1, b"right");
        let plan = fixture.plan().await.metadata;
        let base = fixture.roots(base).await;

        assert_eq!(plan.stats.single_parent_edges, 2);
        assert!(plan.stats.conflicts > 0, "{:?}", plan.stats);
        assert!(!plan.bases.contains_key(&base.tree));
        assert!(!plan.bases.contains_key(&base.content));
        // Both children are tips, so nothing they reach is a target.
        let (tips, _) = fixture.tips(&[left, right]).await;
        assert!(plan.bases.keys().all(|target| !tips.contains(target)));
    }

    #[tokio::test]
    async fn content_history_pairs_changed_files_along_each_edge() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        let one = ChangeId::new();
        let first = fixture.change(base, one, None, 0, b"initial", b"one");
        let second = fixture.change(first, ChangeId::new(), Some(one), 0, b"one", b"two");
        let third = fixture.edit(second, 1, b"other");
        let content = fixture.plan().await.content;

        // Each edge pairs only the file it changed, older with newer.
        assert_eq!(
            content.pairs,
            [
                pair(b"initial", b"one"),
                pair(b"one", b"two"),
                pair(b"initial", b"other"),
            ]
            .into()
        );
        // The tip's files are exactly the kept-whole content.
        assert_eq!(
            content.tips,
            [b"initial".as_slice(), b"two", b"other"]
                .map(ContentId::of)
                .into()
        );
        assert_eq!(content.tips, fixture.tips(&[third]).await.1.tips);
    }

    #[tokio::test]
    async fn edges_into_a_view_with_two_parents_are_not_paired() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        // Views compose changes in ID order. Creating `high` first and then
        // `low` on top of it composes `low` first, so the result is reached
        // both from `high`'s view and from `low`'s own view.
        let mut ids = [ChangeId::new(), ChangeId::new()];
        ids.sort();
        let [low, high] = ids;
        let left = fixture.change(base, high, None, 0, b"initial", b"left");
        let merged = fixture.change(left, low, None, 1, b"initial", b"right");
        let parents = fixture
            .repo
            .view_lineage()
            .filter(|(_, child)| *child == merged)
            .count();
        assert_eq!(parents, 2, "the fixture must build a two-parent view");

        let plan = fixture.plan().await;
        assert_eq!(plan.metadata.stats.views, 4);
        // Only base -> left and base -> low's view; not the merge's two edges.
        assert_eq!(plan.metadata.stats.single_parent_edges, 2);
        assert_eq!(
            plan.content.pairs,
            [pair(b"initial", b"left"), pair(b"initial", b"right")].into()
        );
        // The merged view is the only tip.
        assert_eq!(plan.content.tips, fixture.tips(&[merged]).await.1.tips);
    }

    #[tokio::test]
    async fn planning_is_deterministic_and_bounded() {
        let mut fixture = Fixture::new();
        let mut view = fixture.repo.base_view().id;
        for index in 0..6 {
            view = fixture.edit(view, index, format!("value {index}").as_bytes());
        }
        let first = fixture.plan().await;
        assert!(!first.metadata.bases.is_empty());
        assert_eq!(first, fixture.plan().await);

        let cases = [
            (
                "metadata proposal",
                HistoryPlanLimits {
                    metadata_proposals: first.metadata.stats.proposals as usize - 1,
                    ..LIMITS
                },
            ),
            (
                "content history pair",
                HistoryPlanLimits {
                    content_pairs: first.content.pairs.len() - 1,
                    ..LIMITS
                },
            ),
            (
                "content tip",
                HistoryPlanLimits {
                    content_tips: first.content.tips.len() - 1,
                    ..LIMITS
                },
            ),
        ];
        for (expected, limits) in cases {
            let error = fixture.plan_with(limits).await.unwrap_err();
            assert!(
                matches!(error, HistoryPlanError::Limit { kind, .. } if kind == expected),
                "{expected}: {error}"
            );
        }
    }
}
