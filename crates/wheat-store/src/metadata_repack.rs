//! History-based delta bases for packed metadata.
//!
//! Repack stores older metadata as a delta against its newer counterpart, the
//! same reverse-delta rule content repack uses. This module only proposes
//! bases; identity never changes, and the segment writer still keeps a delta
//! only when it is smaller than the whole record and its base shares the
//! segment.
//!
//! For each view-lineage edge whose child has exactly one parent, the older
//! (parent) view's objects are paired with the newer (child) view's objects at
//! the same position:
//!
//! - directory nodes at the same path, descending only where IDs differ;
//! - revision- and application-map radix nodes at the same edge path;
//! - the views' required-content sets.
//!
//! A target claimed by two different bases, or proposed as its own base, stays
//! whole. Every object reachable from a tip view (a view without children)
//! stays whole, so the newest state never pays for delta resolution.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::{
    CanonicalObjectError, DirectoryEntry, ObjectId, Repository, ViewId,
    decode_application_map_node, decode_content_manifest_record, decode_directory_node,
    decode_revision_map_node, decode_view_record,
};

use crate::{ImmutableObjectStore, ObjectStoreError};

type Digest = [u8; 32];

#[derive(Debug, Error)]
pub(crate) enum MetadataDeltaPlanError {
    #[error(transparent)]
    Store(#[from] ObjectStoreError),
    #[error(transparent)]
    Canonical(#[from] CanonicalObjectError),
    #[error("metadata delta planning exceeds its {limit} proposal limit")]
    Limit { limit: usize },
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
    content_set: Digest,
}

#[derive(Clone, Copy)]
enum MapKind {
    Revision,
    Application,
}

struct Proposals {
    bases: BTreeMap<Digest, Proposal>,
    limit: usize,
}

impl Proposals {
    fn propose(&mut self, target: Digest, base: Digest) -> Result<(), MetadataDeltaPlanError> {
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
            return Err(MetadataDeltaPlanError::Limit { limit: self.limit });
        }
        self.bases.insert(target, proposed);
        Ok(())
    }
}

/// Plans history-based bases for every retained view-lineage edge.
/// `max_proposals` bounds the plan's memory; exceeding it is an error rather
/// than a silently partial plan.
pub(crate) async fn plan_metadata_deltas<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    repository: &Repository,
    max_proposals: usize,
) -> Result<MetadataDeltaPlan, MetadataDeltaPlanError> {
    let mut view_ids = repository
        .retained_views()
        .map(|view| view.id)
        .collect::<BTreeSet<ViewId>>();
    let mut parents = BTreeMap::<ViewId, Vec<ViewId>>::new();
    let mut has_children = BTreeSet::new();
    for (parent, child) in repository.view_lineage() {
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
        limit: max_proposals,
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
        pair_trees(store, &mut proposals, older.tree, newer.tree).await?;
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
        if older.content_set != newer.content_set {
            proposals.propose(older.content_set, newer.content_set)?;
        }
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
            reachable_from_view(store, *roots, &mut tips).await?;
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
    Ok(MetadataDeltaPlan { bases, stats })
}

async fn read<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    id: Digest,
) -> Result<bytes::Bytes, ObjectStoreError> {
    Ok(store.get(ObjectId::from_digest(id)).await?.canonical_bytes)
}

async fn view_roots<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    id: ViewId,
) -> Result<ViewRoots, MetadataDeltaPlanError> {
    let view = decode_view_record(&read(store, *id.as_bytes()).await?)?;
    let manifest =
        decode_content_manifest_record(&read(store, *view.required_content.as_bytes()).await?)?;
    Ok(ViewRoots {
        tree: *view.tree.as_bytes(),
        revisions: *view.revisions.as_bytes(),
        applications: *view.applications.as_bytes(),
        content_set: *manifest.content.as_bytes(),
    })
}

async fn directory_children<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    id: Digest,
) -> Result<BTreeMap<String, Digest>, MetadataDeltaPlanError> {
    Ok(decode_directory_node(&read(store, id).await?)?
        .entries
        .into_iter()
        .filter_map(|(name, entry)| match entry {
            DirectoryEntry::Directory(child) => Some((name, *child.as_bytes())),
            DirectoryEntry::File(_) => None,
        })
        .collect())
}

async fn map_children<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    kind: MapKind,
    id: Digest,
) -> Result<BTreeMap<u8, Digest>, MetadataDeltaPlanError> {
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
    })
}

/// Pairs the older and newer directory at every shared path whose IDs differ.
async fn pair_trees<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    proposals: &mut Proposals,
    older: Digest,
    newer: Digest,
) -> Result<(), MetadataDeltaPlanError> {
    let mut pending = vec![(older, newer)];
    while let Some((older, newer)) = pending.pop() {
        if older == newer {
            continue;
        }
        proposals.propose(older, newer)?;
        let newer_children = directory_children(store, newer).await?;
        for (name, older_child) in directory_children(store, older).await? {
            if let Some(newer_child) = newer_children.get(&name) {
                pending.push((older_child, *newer_child));
            }
        }
    }
    Ok(())
}

/// Pairs the older and newer radix node at every shared edge path whose IDs
/// differ.
async fn pair_maps<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    proposals: &mut Proposals,
    kind: MapKind,
    older: Digest,
    newer: Digest,
) -> Result<(), MetadataDeltaPlanError> {
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

/// Every pairable object reachable from a view: its directories, both maps'
/// radix nodes, and its content set.
async fn reachable_from_view<S: ImmutableObjectStore + ?Sized>(
    store: &S,
    view: ViewRoots,
    into: &mut BTreeSet<Digest>,
) -> Result<(), MetadataDeltaPlanError> {
    let mut trees = vec![view.tree];
    while let Some(tree) = trees.pop() {
        if into.insert(tree) {
            trees.extend(directory_children(store, tree).await?.into_values());
        }
    }
    for (kind, root) in [
        (MapKind::Revision, view.revisions),
        (MapKind::Application, view.applications),
    ] {
        let mut nodes = vec![root];
        while let Some(node) = nodes.pop() {
            if into.insert(node) {
                nodes.extend(map_children(store, kind, node).await?.into_values());
            }
        }
    }
    into.insert(view.content_set);
    Ok(())
}

#[cfg(test)]
mod tests {
    use wheat_core::{
        ApplicationContext, ContentId, EditOperation, EditRecord, EditTransition, FileKind,
        FileValue, RevisionEdits, Tree,
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

        /// Replaces `dir/file{index}` on top of `effective`.
        fn edit(&mut self, effective: ViewId, index: usize, value: &[u8]) -> ViewId {
            self.content.put(value).unwrap();
            let basis = self.repo.base_view().id;
            self.repo
                .create_change(
                    basis,
                    effective,
                    [],
                    replace(&format!("dir/file{index:02}"), b"initial", value),
                    ApplicationContext::new(&self.content),
                )
                .unwrap()
                .1
                .resulting_view
        }

        async fn plan(&self) -> MetadataDeltaPlan {
            for object in self.repo.canonical_closure().objects {
                self.objects
                    .put_if_absent(object.id, &object.bytes)
                    .await
                    .unwrap();
            }
            plan_metadata_deltas(&self.objects, &self.repo, 10_000)
                .await
                .unwrap()
        }

        async fn roots(&self, view: ViewId) -> ViewRoots {
            view_roots(&self.objects, view).await.unwrap()
        }
    }

    #[tokio::test]
    async fn linear_history_pairs_each_older_view_with_its_successor() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        let first = fixture.edit(base, 0, b"one");
        let second = fixture.edit(first, 1, b"two");
        let plan = fixture.plan().await;
        let [base, first, second] = [
            fixture.roots(base).await,
            fixture.roots(first).await,
            fixture.roots(second).await,
        ];

        assert_eq!(plan.stats.views, 3);
        assert_eq!(plan.stats.single_parent_edges, 2);
        assert_eq!(plan.stats.conflicts, 0);
        // Older roots delta against the next view's roots.
        assert_eq!(plan.bases.get(&base.tree), Some(&first.tree));
        assert_eq!(plan.bases.get(&first.tree), Some(&second.tree));
        assert_eq!(plan.bases.get(&base.content_set), Some(&first.content_set));
        assert_eq!(
            plan.bases.get(&first.content_set),
            Some(&second.content_set)
        );
        // The tip view stays whole.
        let mut tip = BTreeSet::new();
        reachable_from_view(&fixture.objects, second, &mut tip)
            .await
            .unwrap();
        assert!(plan.bases.keys().all(|target| !tip.contains(target)));
        assert!(plan.bases.iter().all(|(target, base)| target != base));
    }

    #[tokio::test]
    async fn siblings_claiming_one_parent_leave_it_whole() {
        let mut fixture = Fixture::new();
        let base = fixture.repo.base_view().id;
        let left = fixture.edit(base, 0, b"left");
        let right = fixture.edit(base, 1, b"right");
        let plan = fixture.plan().await;
        let base = fixture.roots(base).await;

        assert_eq!(plan.stats.single_parent_edges, 2);
        assert!(plan.stats.conflicts > 0, "{:?}", plan.stats);
        assert!(!plan.bases.contains_key(&base.tree));
        assert!(!plan.bases.contains_key(&base.content_set));
        // Both children are tips, so nothing they reach is a target.
        let mut tips = BTreeSet::new();
        for view in [left, right] {
            let roots = fixture.roots(view).await;
            reachable_from_view(&fixture.objects, roots, &mut tips)
                .await
                .unwrap();
        }
        assert!(plan.bases.keys().all(|target| !tips.contains(target)));
    }

    #[tokio::test]
    async fn planning_is_deterministic_and_bounded() {
        let mut fixture = Fixture::new();
        let mut view = fixture.repo.base_view().id;
        for index in 0..6 {
            view = fixture.edit(view, index, format!("value {index}").as_bytes());
        }
        let first = fixture.plan().await;
        assert!(!first.bases.is_empty());
        assert_eq!(first, fixture.plan().await);

        let limit = first.stats.proposals as usize - 1;
        assert!(matches!(
            plan_metadata_deltas(&fixture.objects, &fixture.repo, limit).await,
            Err(MetadataDeltaPlanError::Limit { limit: reported }) if reported == limit
        ));
    }
}
