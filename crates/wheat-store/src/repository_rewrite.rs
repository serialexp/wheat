//! Explicit, sealed non-monotonic history rewrites.
//!
//! This module is intentionally separate from ordinary repository mutation.  A rewrite is planned
//! against exact authenticated selections, proves that the effective meaning is unchanged, builds
//! the retained inventories from caller-supplied authority roots, and performs both transition and
//! complete validation before returning a value which can be installed by one CAS.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use thiserror::Error;
use wheat_core::*;

use crate::{
    ApplicationMapStore, ConflictSetStore, ContentSetStore, ContentStore, ImmutableObjectStore,
    MerkleTreeStore, PublicationError, PublicationStore, RepositoryHead, RepositoryMutationError,
    RepositoryPublicationError, RepositoryPublisher, RepositoryReader, RetainedSetStore,
    RevisionMapStore, TransitionValidationOptions, TreeContentWalkLimits, ViewLineageMapStore,
    repository_mutation::{Fresh, PendingRevision, PersistedRepositoryMutator},
};

#[derive(Clone, Debug)]
pub struct SquashRewriteRequest {
    /// The exact current revision of every selected change.  Supplying the whole frontier makes
    /// stale planning detectable even when an affected descendant changed concurrently.
    pub expected_revisions: BTreeMap<ChangeId, RevisionId>,
    /// Changes whose checkpoint suffix is collapsed, and the predecessor retained at its boundary.
    pub retained_predecessors: BTreeMap<ChangeId, Option<RevisionId>>,
    /// Honest metadata for each newly-authored collapsed revision.
    pub revision_metadata: BTreeMap<ChangeId, RevisionMetadata>,
    /// Views selected by workspaces, labels, snapshots, or another point authority.
    pub authority_roots: BTreeSet<ViewId>,
    /// Complete retained-view inventories owned by another authority, such as the exact observed
    /// remote state. These are expanded fail-closed while constructing the replacement closure.
    pub authority_view_sets: BTreeSet<RetainedSetId>,
    /// Complete retained-revision inventories owned by another authority. In particular, routine
    /// squash preserves every revision retained by the exact observed remote state, not only
    /// revisions reachable from its current labels.
    pub authority_revision_sets: BTreeSet<RetainedSetId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RewriteMapping {
    pub revisions: BTreeMap<RevisionId, RevisionId>,
    pub views: BTreeMap<ViewId, ViewId>,
    pub reapplied: Vec<ChangeId>,
}

pub struct PreparedRepositoryRewrite {
    expected: RepositoryHead,
    proposed: RepositoryHead,
    state: RepositoryStateId,
    resulting_view: ViewId,
    mapping: RewriteMapping,
    _sealed: (),
}

impl PreparedRepositoryRewrite {
    #[must_use]
    pub fn expected(&self) -> RepositoryHead {
        self.expected
    }
    #[must_use]
    pub fn proposed(&self) -> RepositoryHead {
        self.proposed
    }
    #[must_use]
    pub fn state(&self) -> RepositoryStateId {
        self.state
    }
    #[must_use]
    pub fn resulting_view(&self) -> ViewId {
        self.resulting_view
    }
    #[must_use]
    pub fn mapping(&self) -> &RewriteMapping {
        &self.mapping
    }
}

#[derive(Debug, Error)]
pub enum RepositoryRewriteError {
    #[error(transparent)]
    Mutation(#[from] RepositoryMutationError),
    #[error(transparent)]
    Publication(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Model(#[from] RepositoryError),
    #[error(transparent)]
    Collection(#[from] crate::CollectionError),
    #[error(transparent)]
    ChangeMap(#[from] crate::ChangeMapError),
    #[error(transparent)]
    Objects(#[from] crate::ObjectStoreError),
    #[error(transparent)]
    Trees(#[from] crate::MerkleTreeError),
    #[error(transparent)]
    RevisionMap(#[from] crate::RevisionMapError),
    #[error(transparent)]
    ApplicationMap(#[from] crate::ApplicationMapError),
    #[error(transparent)]
    ViewLineage(#[from] crate::ViewLineageError),
    #[error("rewrite request does not match the exact current revision selection")]
    StaleSelection,
    #[error("rewrite request references a change outside the exact current selection")]
    UnknownRequestChange,
    #[error("rewrite metadata was supplied for a change which is not being collapsed")]
    UnexpectedMetadata,
    #[error("rewrite predecessor is not an ancestor of the selected revision")]
    InvalidBoundary,
    #[error("rewrite boundary is the selected current revision")]
    BoundaryIsCurrent,
    /// A combined revision's edits are exactly the merge of the revisions it combines,
    /// which validation checks, so a rewrite never collapses one.
    #[error("rewrite would collapse a revision that combines others")]
    CollapsesCombinedRevision,
    #[error("the line a squash would collapse has more than {limit} revisions")]
    LineTooLong { limit: usize },
    #[error("rewrite metadata is missing for a collapsed change")]
    MissingMetadata,
    #[error("collapsed revision evidence cannot be represented without losing provenance")]
    UnsupportedEvidence,
    #[error("rewrite changed effective repository semantics")]
    SemanticDrift,
    #[error("resolved conflict application does not have exactly one retained resulting view")]
    MissingProducedView,
    #[error("authority root is not retained by the current repository")]
    UnknownAuthorityRoot,
    /// An annotated label is a (possibly already pushed) Git tag, so its view must
    /// never move. Delete the label first if the rewrite is really wanted.
    #[error("rewrite would move annotated label {0}, which is published as a Git tag")]
    MovesAnnotatedLabel(String),
}

type Result<T> = std::result::Result<T, RepositoryRewriteError>;

pub struct RepositoryRewriter<'a, O, P> {
    objects: &'a O,
    content: &'a ContentStore,
    publications: &'a P,
}

impl<'a, O: ImmutableObjectStore, P: PublicationStore> RepositoryRewriter<'a, O, P> {
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore, publications: &'a P) -> Self {
        Self {
            objects,
            content,
            publications,
        }
    }

    pub async fn prepare_squash(
        &self,
        expected: RepositoryHead,
        basis_view: ViewId,
        effective_view: ViewId,
        request: SquashRewriteRequest,
    ) -> Result<Option<PreparedRepositoryRewrite>> {
        if request.retained_predecessors.is_empty() {
            return if request.revision_metadata.is_empty() {
                Ok(None)
            } else {
                Err(RepositoryRewriteError::UnexpectedMetadata)
            };
        }
        // A squash checks and rewrites against the whole selection, so it loads all of it.
        let opener = PersistedRepositoryMutator::new(self.objects, self.content, self.publications);
        let mut active = opener.open(expected, basis_view, effective_view).await?;
        opener.load_from(&mut active, 0).await?;
        let actual: BTreeMap<ChangeId, RevisionId> = active
            .revisions
            .iter()
            .map(|(c, (r, _))| (*c, *r))
            .collect();
        if request
            .retained_predecessors
            .keys()
            .chain(request.revision_metadata.keys())
            .any(|change| !active.revisions.contains_key(change))
        {
            return Err(RepositoryRewriteError::UnknownRequestChange);
        }
        if actual != request.expected_revisions {
            return Err(RepositoryRewriteError::StaleSelection);
        }
        if request
            .revision_metadata
            .keys()
            .any(|change| !request.retained_predecessors.contains_key(change))
        {
            return Err(RepositoryRewriteError::UnexpectedMetadata);
        }
        if request
            .retained_predecessors
            .keys()
            .any(|change| !request.revision_metadata.contains_key(change))
        {
            return Err(RepositoryRewriteError::MissingMetadata);
        }

        let reader = RepositoryReader::new(self.objects, self.content);
        let retained = RetainedSetStore::new(self.objects);
        for root in &request.authority_roots {
            if !retained
                .contains(active.state.retained_views, (*root).into())
                .await?
            {
                return Err(RepositoryRewriteError::UnknownAuthorityRoot);
            }
        }
        // Authority inventories must themselves be authenticated members of the current object
        // closure. A caller cannot smuggle in an arbitrary trie root and make unrelated objects
        // authoritative merely because those objects happen to exist in the backend.
        let mut retained_nodes = retained.cursor(active.state.retained_views).await?;
        let mut current_views = BTreeSet::new();
        while let Some(id) = retained_nodes.next().await? {
            current_views.insert(id);
        }
        let mut retained_nodes = retained.cursor(active.state.retained_revisions).await?;
        let mut current_revisions = BTreeSet::new();
        while let Some(id) = retained_nodes.next().await? {
            current_revisions.insert(id);
        }
        let mut authority_views = BTreeSet::new();
        for set in &request.authority_view_sets {
            let mut cursor = retained.cursor(*set).await?;
            while let Some(id) = cursor.next().await? {
                if !current_views.contains(&id) {
                    return Err(RepositoryRewriteError::UnknownAuthorityRoot);
                }
                authority_views.insert(ViewId::from_digest(*id.as_bytes()));
            }
        }
        let mut authority_revisions = BTreeSet::new();
        for set in &request.authority_revision_sets {
            let mut cursor = retained.cursor(*set).await?;
            while let Some(id) = cursor.next().await? {
                if !current_revisions.contains(&id) {
                    return Err(RepositoryRewriteError::UnknownAuthorityRoot);
                }
                authority_revisions.insert(RevisionId::from_digest(*id.as_bytes()));
            }
        }

        let old_effective = active.effective;
        let reverse_dependencies = reverse_dependencies(&active.revisions)?;
        let affected = collect_descendants(
            request.retained_predecessors.keys().copied(),
            &reverse_dependencies,
        );
        let order = topo(&active.revisions, &affected, &reverse_dependencies)?;
        let mut fresh = Fresh::default();
        let mut revision_mapping = BTreeMap::new();
        let mutator =
            PersistedRepositoryMutator::new(self.objects, self.content, self.publications);

        let mut pending = BTreeMap::new();
        let mut collapsed = BTreeMap::new();
        for change in &order {
            let (old_id, old) = active.revisions[change].clone();
            // A dependency rewrite changes where descendants are applied, not what their revision
            // was authored to mean. Keep that clean authored revision byte-for-byte and let compose
            // produce the (possibly conflicted) descendant application against the new dependency.
            let Some(boundary) = request.retained_predecessors.get(change) else {
                revision_mapping.insert(old_id, old_id);
                continue;
            };
            validate_boundary(&reader, old_id, *boundary).await?;
            let predecessor = *boundary;
            let metadata = request
                .revision_metadata
                .get(change)
                .cloned()
                .unwrap_or_else(|| old.metadata.clone());
            let edit_ids = old.edits.clone();
            let predecessor_edits = if let Some(id) = predecessor {
                reader.revision(id).await?.edits
            } else {
                BTreeSet::new()
            };
            let preserved = edit_ids.intersection(&predecessor_edits).copied().collect();
            let added: BTreeSet<_> = edit_ids.difference(&predecessor_edits).copied().collect();
            let removed: BTreeSet<_> = predecessor_edits.difference(&edit_ids).copied().collect();
            let (subsumed, resolutions) = collapsed_evidence(
                &reader,
                old_id,
                predecessor,
                &predecessor_edits,
                &added,
                &removed,
                &edit_ids,
            )
            .await?;
            pending.insert(
                *change,
                PendingRevision {
                    predecessor,
                    dependencies: old.dependencies.clone(),
                    metadata,
                    edits: RevisionEdits {
                        edits: mutator.load_edits(&old).await?,
                        transition: EditTransition {
                            preserved,
                            added,
                            removed,
                            subsumed,
                            resolutions,
                        },
                    },
                },
            );
            collapsed.insert(*change, old_id);
        }

        // Each collapsed revision keeps its edits and is re-authored at its spot, like any other
        // revision. Its edits applied cleanly there before, so failing now means the rewrite
        // would change what the change means.
        let (resulting, authored) = match mutator
            .compose_authoring(&mut active, pending, &mut fresh)
            .await
        {
            Ok(composed) => composed,
            Err(RepositoryMutationError::Model(RepositoryError::InvalidEdits(_))) => {
                return Err(RepositoryRewriteError::SemanticDrift);
            }
            Err(error) => return Err(error.into()),
        };
        for (change, old_id) in collapsed {
            revision_mapping.insert(old_id, authored[&change]);
        }
        if semantic_view(&reader, &old_effective).await?
            != semantic_view(&reader, &resulting).await?
        {
            return Err(RepositoryRewriteError::SemanticDrift);
        }
        let mut view_mapping = BTreeMap::new();
        let old_applications = ApplicationMapStore::new(self.objects);
        let new_applications = ApplicationMapStore::new(self.objects);
        for change in &order {
            let old_application = old_applications
                .get(old_effective.applications, *change)
                .await?
                .ok_or(RepositoryRewriteError::SemanticDrift)?;
            let new_application = new_applications
                .get(resulting.applications, *change)
                .await?
                .ok_or(RepositoryRewriteError::SemanticDrift)?;
            let old_target = reader.application(old_application).await?.target_view;
            let new_target = reader.application(new_application).await?.target_view;
            if let Some(previous) = view_mapping.insert(old_target, new_target)
                && previous != new_target
            {
                return Err(RepositoryRewriteError::SemanticDrift);
            }
        }
        view_mapping.insert(effective_view, resulting.id());

        // Checked before any state is built: an annotated label may already be a
        // pushed Git tag, and moving it would change the commit the tag names.
        let old_envelope = reader.envelope(expected.envelope).await?;
        let old_labels = reader.view_label_manifest(old_envelope.labels).await?;
        if let Some((name, _)) = old_labels.labels.iter().find(|(_, label)| {
            label.annotation.is_some()
                && view_mapping
                    .get(&label.view)
                    .is_some_and(|mapped| *mapped != label.view)
        }) {
            return Err(RepositoryRewriteError::MovesAnnotatedLabel(name.clone()));
        }

        // Authority references move with an exactly equivalent rewritten view. Keeping an old
        // label/workspace target as an authority root would defeat collapse by retaining the
        // replaced suffix solely because its reference had not been updated yet.
        let mapped_authority_roots = request
            .authority_roots
            .iter()
            .map(|root| view_mapping.get(root).copied().unwrap_or(*root))
            .collect();
        let state = self
            .build_state(
                &active.state,
                resulting.id(),
                &mapped_authority_roots,
                &authority_views,
                &authority_revisions,
                &fresh,
            )
            .await?;
        self.objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await?;
        reader
            .validate_transition_with_options(
                active.state.id(),
                state.id(),
                TransitionValidationOptions {
                    allow_non_monotonic: true,
                },
            )
            .await?;
        reader.validate_complete(state.id()).await?;

        let labels =
            ViewLabelManifestRecord::new(old_labels.labels.into_iter().map(|(name, mut label)| {
                label.view = view_mapping.get(&label.view).copied().unwrap_or(label.view);
                (name, label)
            }))
            .map_err(crate::ObjectStoreError::from)?;
        self.objects
            .put_if_absent(labels.id().into(), &labels.canonical_bytes())
            .await?;
        for label in labels.labels.values() {
            if !retained
                .contains(state.retained_views, label.view.into())
                .await?
            {
                return Err(RepositoryRewriteError::SemanticDrift);
            }
        }
        let envelope = RepositoryPublisher::new(self.objects, self.content, self.publications)
            .prepare_envelope_unchecked(state.id(), labels.id())
            .await?;
        let proposed = expected
            .next(envelope)
            .map_err(RepositoryPublicationError::from)?;
        Ok(Some(PreparedRepositoryRewrite {
            expected,
            proposed,
            state: state.id(),
            resulting_view: resulting.id(),
            mapping: RewriteMapping {
                revisions: revision_mapping,
                views: view_mapping,
                reapplied: order,
            },
            _sealed: (),
        }))
    }

    /// The one view in `old`'s lineage that `application` produced from its target: the
    /// target's child with the application's change last, selecting it. Full validation
    /// identifies the composition edge the same way.
    async fn produced_view(
        &self,
        old: &RepositoryStateRecord,
        application_id: ApplicationId,
    ) -> Result<ViewId> {
        let reader = RepositoryReader::new(self.objects, self.content);
        let application = reader.application(application_id).await?;
        let change = reader.revision(application.revision_id).await?.change_id;
        let children = ViewLineageMapStore::new(self.objects)
            .get(old.view_lineage, application.target_view)
            .await?
            .ok_or(RepositoryRewriteError::MissingProducedView)?;
        let maps = ApplicationMapStore::new(self.objects);
        let mut produced = None;
        let mut cursor = RetainedSetStore::new(self.objects).cursor(children).await?;
        while let Some(child) = cursor.next().await? {
            let child = ViewId::from_digest(*child.as_bytes());
            let view = reader.view(child).await?;
            if view.order.last == Some(change)
                && maps.get(view.applications, change).await? == Some(application_id)
                && produced.replace(child).is_some()
            {
                return Err(RepositoryRewriteError::MissingProducedView);
            }
        }
        produced.ok_or(RepositoryRewriteError::MissingProducedView)
    }

    async fn build_state(
        &self,
        old: &RepositoryStateRecord,
        result: ViewId,
        roots: &BTreeSet<ViewId>,
        authority_views: &BTreeSet<ViewId>,
        authority_revisions: &BTreeSet<RevisionId>,
        fresh: &Fresh,
    ) -> Result<RepositoryStateRecord> {
        let reader = RepositoryReader::new(self.objects, self.content);
        let mut views = roots.clone();
        views.extend(authority_views);
        views.insert(old.base_view);
        views.insert(result);
        let mut revisions = authority_revisions.clone();
        let mut applications = BTreeSet::new();
        let mut view_queue: VecDeque<_> = views.iter().copied().collect();
        let mut revision_queue = VecDeque::new();
        let mut application_queue = VecDeque::new();
        while !view_queue.is_empty() || !revision_queue.is_empty() || !application_queue.is_empty()
        {
            while let Some(view_id) = view_queue.pop_front() {
                let view = reader.view(view_id).await?;
                let mut rc = RevisionMapStore::new(self.objects)
                    .cursor(view.revisions)
                    .await?;
                while let Some((_, id)) = rc.next().await? {
                    if revisions.insert(id) {
                        revision_queue.push_back(id);
                    }
                }
                let mut ac = ApplicationMapStore::new(self.objects)
                    .cursor(view.applications)
                    .await?;
                while let Some((_, id)) = ac.next().await? {
                    if applications.insert(id) {
                        application_queue.push_back(id);
                    }
                }
            }
            while let Some(id) = revision_queue.pop_front() {
                let revision = reader.revision(id).await?;
                if views.insert(revision.authored_against) {
                    view_queue.push_back(revision.authored_against);
                }
                for parent in revision.predecessor.iter().chain(&revision.combines) {
                    if revisions.insert(*parent) {
                        revision_queue.push_back(*parent);
                    }
                }
                for resolution in revision.transition.resolutions {
                    if applications.insert(resolution.application) {
                        application_queue.push_back(resolution.application);
                        // A resolved conflicted application is not selected by any view on the
                        // retained chain, so the view it produced is only reachable from here.
                        // A retained application must keep its exactly one lineage edge.
                        let produced = self.produced_view(old, resolution.application).await?;
                        if views.insert(produced) {
                            view_queue.push_back(produced);
                        }
                    }
                }
            }
            while let Some(id) = application_queue.pop_front() {
                let application = reader.application(id).await?;
                if revisions.insert(application.revision_id) {
                    revision_queue.push_back(application.revision_id);
                }
                if views.insert(application.target_view) {
                    view_queue.push_back(application.target_view);
                }
            }
        }
        views.extend(&fresh.views);
        revisions.extend(&fresh.revisions);
        applications.extend(&fresh.applications);
        let sets = RetainedSetStore::new(self.objects);
        let empty = sets.empty_root().await?;
        let retained_views = sets
            .update_batch(empty, views.iter().map(|v| ((*v).into(), true)))
            .await?;
        let retained_revisions = sets
            .update_batch(empty, revisions.iter().map(|r| ((*r).into(), true)))
            .await?;
        let retained_applications = sets
            .update_batch(empty, applications.iter().map(|a| ((*a).into(), true)))
            .await?;
        let mut changes = Vec::new();
        let mut cc = crate::ChangeMapStore::new(self.objects)
            .cursor(old.changes)
            .await?;
        while let Some((_, o)) = cc.next().await? {
            changes.push((o.into(), true));
        }
        let retained_changes = sets.update_batch(empty, changes).await?;
        let content_sets = ContentSetStore::new(self.objects);
        let trees = MerkleTreeStore::new(self.objects);
        let mut content_ids = BTreeSet::new();
        let mut content_trees = BTreeSet::new();
        for view_id in &views {
            content_trees.insert(reader.view(*view_id).await?.tree);
        }
        let mut loaded_revisions = Vec::with_capacity(revisions.len());
        for revision_id in &revisions {
            let revision = reader.revision(*revision_id).await?;
            content_trees.insert(revision.resulting_tree);
            loaded_revisions.push(revision);
        }
        // Application results and conflict evidence name content too, and validation
        // requires it in the aggregate like core's canonical closure does.
        for application_id in &applications {
            match reader.application(*application_id).await?.result {
                ApplicationResultRecord::Clean { tree } => {
                    content_trees.insert(tree);
                }
                ApplicationResultRecord::Conflicted {
                    partial_tree,
                    conflict,
                } => {
                    content_trees.insert(partial_tree);
                    for evidence in reader.conflict(conflict).await?.edits.values() {
                        content_ids.extend(evidence.target.map(|value| value.content));
                    }
                }
            }
        }
        for tree in content_trees {
            let mut cursor = trees
                .content_cursor(tree, TreeContentWalkLimits::default())
                .await?;
            while let Some(value) = cursor.next().await? {
                content_ids.insert(value.content);
            }
        }
        for revision in loaded_revisions {
            for edit in revision.edits {
                match reader.edit(edit).await?.operation {
                    EditOperation::GuardedText {
                        before,
                        expected,
                        replacement,
                        after,
                        ..
                    } => {
                        content_ids.extend([before, expected, replacement, after]);
                    }
                    EditOperation::ExactFile {
                        expected,
                        replacement,
                    } => {
                        content_ids
                            .extend(expected.into_iter().chain(replacement).map(|v| v.content));
                    }
                }
            }
        }
        let required_content = content_sets
            .update_batch(
                content_sets.empty_root().await?,
                content_ids.into_iter().map(|id| (id, true)),
            )
            .await?;

        let lineage = ViewLineageMapStore::new(self.objects);
        let mut edges = BTreeSet::new();
        for parent in &views {
            if let Some(children) = lineage.get(old.view_lineage, *parent).await? {
                let mut c = sets.cursor(children).await?;
                while let Some(child) = c.next().await? {
                    let child = ViewId::from_digest(*child.as_bytes());
                    if views.contains(&child) {
                        edges.insert((*parent, child));
                    }
                }
            }
        }
        edges.extend(
            fresh
                .view_edges
                .iter()
                .filter(|(p, c)| views.contains(p) && views.contains(c))
                .copied(),
        );
        let view_lineage = lineage
            .insert_edges(lineage.empty_root().await?, edges)
            .await?;
        // A dropped application's commit is no longer recognised: a pull imports it.
        let projected = crate::ProjectedCommitSetStore::new(self.objects);
        let mut kept = Vec::new();
        let mut members = projected.cursor(old.projected_commits).await?;
        while let Some(member) = members.next().await? {
            if applications.contains(&member.application()) {
                kept.push((member, true));
            }
        }
        let projected_commits = projected
            .update_batch(projected.empty_root().await?, kept)
            .await?;
        Ok(RepositoryStateRecord {
            base_view: old.base_view,
            changes: old.changes,
            view_lineage,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
            projected_commits,
        })
    }
}

pub fn finalize_repository_rewrite<P: PublicationStore>(
    publications: &P,
    prepared: PreparedRepositoryRewrite,
) -> Result<RepositoryHead> {
    publications
        .compare_and_advance(prepared.expected, prepared.proposed)
        .map_err(|e| match e {
            PublicationError::Stale { .. } => {
                RepositoryRewriteError::Mutation(RepositoryMutationError::Stale {
                    expected: prepared.expected,
                    actual: publications.read().unwrap_or(prepared.expected),
                })
            }
            other => RepositoryRewriteError::Publication(other.into()),
        })?;
    Ok(prepared.proposed)
}

async fn validate_boundary<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    current: RevisionId,
    boundary: Option<RevisionId>,
) -> Result<()> {
    if boundary == Some(current) {
        return Err(RepositoryRewriteError::BoundaryIsCurrent);
    }
    let mut at = Some(current);
    while let Some(id) = at {
        if Some(id) == boundary {
            return Ok(());
        }
        let record = reader.revision(id).await?;
        if !record.combines.is_empty() {
            return Err(RepositoryRewriteError::CollapsesCombinedRevision);
        }
        at = record.predecessor;
    }
    if boundary.is_some() {
        return Err(RepositoryRewriteError::InvalidBoundary);
    }
    Ok(())
}

/// What a squash of one change's line keeps besides its latest revision.
#[derive(Clone, Copy, Debug)]
pub struct SquashStops<'s> {
    /// The change's revision in the workspace's basis.
    pub basis: Option<RevisionId>,
    /// A retained-revision set whose members stay, such as the observed remote state's.
    pub published: Option<RetainedSetId>,
    /// The change's revisions in the other views that stay retained: other workspaces'
    /// views, labels.
    pub held: &'s [RevisionId],
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// The predecessor a squash of the line ending at `latest` collapses onto: the
    /// latest revision on its predecessor chain that is the basis's, is published,
    /// combines revisions, or is a branch point, one a held revision descends from.
    /// Collapsing a branch point would erase the removals a later merge with the line
    /// branching off it needs. `None` collapses the whole line; `Some(latest)`, nothing.
    ///
    /// Reads the line down to the boundary, and from each held revision the revisions
    /// no lower than the line's lowest generation.
    pub async fn squash_boundary(
        &self,
        latest: RevisionId,
        stops: SquashStops<'_>,
        max_revisions: usize,
    ) -> Result<Option<RevisionId>> {
        let retained = RetainedSetStore::new(self.objects);
        // The revisions a squash may collapse, by their generation.
        let mut line = BTreeMap::new();
        let mut at = latest;
        let stop = loop {
            if Some(at) == stops.basis {
                break Some(at);
            }
            if let Some(published) = stops.published
                && retained.contains(published, at.into()).await?
            {
                break Some(at);
            }
            let record = self.revision(at).await?;
            if !record.combines.is_empty() {
                break Some(at);
            }
            if line.len() >= max_revisions {
                return Err(RepositoryRewriteError::LineTooLong {
                    limit: max_revisions,
                });
            }
            line.insert(at, record.generation);
            match record.predecessor {
                Some(predecessor) => at = predecessor,
                None => break None,
            }
        };
        let Some(lowest) = line.values().min().copied() else {
            return Ok(stop);
        };
        let mut branch: Option<(u64, RevisionId)> = None;
        for held in stops.held {
            if *held == latest {
                continue;
            }
            let mut pending = vec![*held];
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !seen.insert(id) {
                    continue;
                }
                if let Some(generation) = line.get(&id) {
                    branch = branch.max(Some((*generation, id)));
                    continue;
                }
                let record = self.revision(id).await?;
                if record.generation > lowest {
                    pending.extend(record.predecessor.iter().chain(&record.combines));
                }
            }
        }
        Ok(branch.map(|(_, id)| Some(id)).unwrap_or(stop))
    }
}

/// Composes the transition evidence of a collapsed checkpoint suffix into the evidence that is
/// still semantically required at the retained boundary.
///
/// Evidence only means something relative to the boundary predecessor, because every checkpoint
/// in the suffix is replaced. So the suffix is replayed oldest first while tracking what each
/// retained term turned into:
///
/// - a boundary edit `s` that a checkpoint subsumes starts a lineage `s -> R`; a later
///   subsumption of any member of `R` substitutes that member's replacements, and a later plain
///   removal drops the member. The collapsed revision records `s -> R` only when `s` is still
///   removed and something of `R` survives. A boundary edit that was re-added, or whose
///   replacements were all removed again, needs no evidence;
/// - a subsumption whose source was itself introduced inside the suffix is an intermediate step:
///   it only rewrites existing lineages and leaves no record of its own;
/// - a conflict resolution is kept when its conflicted application belongs to a revision outside
///   the suffix (the conflict is still real at the boundary), with its replacement edits mapped
///   through the same lineage. A resolution of an intermediate checkpoint's application is
///   dropped: the collapsed revision is re-authored at its spot and must apply cleanly there.
///
/// Anything that still cannot be expressed as a direct transition from the boundary fails
/// closed with [`RepositoryRewriteError::UnsupportedEvidence`] rather than being erased.
async fn collapsed_evidence<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    current: RevisionId,
    boundary: Option<RevisionId>,
    boundary_edits: &BTreeSet<EditId>,
    added: &BTreeSet<EditId>,
    removed: &BTreeSet<EditId>,
    selected: &BTreeSet<EditId>,
) -> Result<(Vec<EditSubsumption>, Vec<ConflictResolutionEvidence>)> {
    let mut suffix = Vec::new();
    let mut suffix_ids = BTreeSet::new();
    let mut at = Some(current);
    while at != boundary {
        let id = at.ok_or(RepositoryRewriteError::InvalidBoundary)?;
        let revision = reader.revision(id).await?;
        at = revision.predecessor;
        suffix_ids.insert(id);
        suffix.push(revision);
    }
    suffix.reverse();

    let mut lineage = Lineage::default();
    let mut kept_resolutions = Vec::new();
    for revision in suffix {
        let transition = revision.transition;
        let mut subsumed_sources = BTreeSet::new();
        for evidence in transition.subsumed {
            subsumed_sources.insert(evidence.source);
            lineage.substitute(evidence.source, &evidence.replacements);
            if boundary_edits.contains(&evidence.source) {
                lineage.extend(Term::Source(evidence.source), &evidence.replacements);
            }
        }
        for edit in transition.removed.difference(&subsumed_sources) {
            lineage.remove(*edit);
        }
        for evidence in transition.resolutions {
            let application = reader.application(evidence.application).await?;
            if suffix_ids.contains(&application.revision_id) {
                continue;
            }
            // An initial revision cannot carry resolution evidence, so a conflict that is still
            // real without any retained predecessor has no direct representation.
            if boundary.is_none() {
                return Err(RepositoryRewriteError::UnsupportedEvidence);
            }
            let term = Term::Resolution(kept_resolutions.len());
            lineage.extend(term, &evidence.replacement_edits);
            kept_resolutions.push(evidence);
        }
    }

    let mut subsumed = Vec::new();
    for (term, replacements) in &lineage.members {
        let Term::Source(source) = term else {
            continue;
        };
        if !removed.contains(source) || replacements.is_empty() {
            continue;
        }
        if !replacements.is_subset(added) {
            return Err(RepositoryRewriteError::UnsupportedEvidence);
        }
        subsumed.push(EditSubsumption {
            source: *source,
            replacements: replacements.clone(),
        });
    }
    let mut resolutions: Vec<ConflictResolutionEvidence> = Vec::new();
    for (index, mut evidence) in kept_resolutions.into_iter().enumerate() {
        evidence.replacement_edits = lineage
            .members
            .remove(&Term::Resolution(index))
            .unwrap_or_default();
        if !evidence.replacement_edits.is_subset(selected) {
            return Err(RepositoryRewriteError::UnsupportedEvidence);
        }
        if !resolutions.contains(&evidence) {
            resolutions.push(evidence);
        }
    }
    Ok((subsumed, resolutions))
}

/// A retained term whose current edits are tracked through a collapsed suffix.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Term {
    /// A boundary edit that a checkpoint subsumed.
    Source(EditId),
    /// A kept conflict resolution, by its index in suffix order.
    Resolution(usize),
}

/// The edits each tracked term currently maps to, with a reverse index so a substitution or
/// removal touches only the terms that hold the edit instead of scanning every lineage.
#[derive(Default)]
struct Lineage {
    members: BTreeMap<Term, BTreeSet<EditId>>,
    holders: BTreeMap<EditId, BTreeSet<Term>>,
}

impl Lineage {
    fn extend(&mut self, term: Term, edits: &BTreeSet<EditId>) {
        self.members.entry(term).or_default().extend(edits);
        for edit in edits {
            self.holders.entry(*edit).or_default().insert(term);
        }
    }

    fn substitute(&mut self, edit: EditId, replacements: &BTreeSet<EditId>) {
        for term in self.holders.remove(&edit).unwrap_or_default() {
            if let Some(members) = self.members.get_mut(&term) {
                members.remove(&edit);
            }
            self.extend(term, replacements);
        }
    }

    fn remove(&mut self, edit: EditId) {
        for term in self.holders.remove(&edit).unwrap_or_default() {
            if let Some(members) = self.members.get_mut(&term) {
                members.remove(&edit);
            }
        }
    }
}

fn reverse_dependencies(
    records: &BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
) -> Result<BTreeMap<ChangeId, Vec<ChangeId>>> {
    let mut reverse = BTreeMap::<_, Vec<_>>::new();
    for change in records.keys() {
        reverse.entry(*change).or_default();
    }
    for (change, (_, record)) in records {
        for dependency in &record.dependencies {
            if !records.contains_key(dependency) {
                return Err(RepositoryError::UnknownDependency(*dependency).into());
            }
            reverse.entry(*dependency).or_default().push(*change);
        }
    }
    Ok(reverse)
}

fn collect_descendants(
    roots: impl IntoIterator<Item = ChangeId>,
    reverse: &BTreeMap<ChangeId, Vec<ChangeId>>,
) -> BTreeSet<ChangeId> {
    let mut found = BTreeSet::new();
    let mut pending: Vec<_> = roots.into_iter().collect();
    while let Some(change) = pending.pop() {
        if !found.insert(change) {
            continue;
        }
        if let Some(children) = reverse.get(&change) {
            pending.extend(children.iter().copied());
        }
    }
    found
}

fn topo(
    records: &BTreeMap<ChangeId, (RevisionId, RevisionRecord)>,
    selected: &BTreeSet<ChangeId>,
    reverse: &BTreeMap<ChangeId, Vec<ChangeId>>,
) -> Result<Vec<ChangeId>> {
    let mut indegree = BTreeMap::new();
    for change in selected {
        let record = records
            .get(change)
            .ok_or(RepositoryError::UnknownDependency(*change))?;
        indegree.insert(
            *change,
            record.1.dependencies.intersection(selected).count(),
        );
    }
    let mut ready: BTreeSet<_> = indegree
        .iter()
        .filter_map(|(change, degree)| (*degree == 0).then_some(*change))
        .collect();
    let mut ordered = Vec::with_capacity(selected.len());
    while let Some(change) = ready.pop_first() {
        ordered.push(change);
        if let Some(children) = reverse.get(&change) {
            for child in children.iter().filter(|child| selected.contains(child)) {
                let degree = indegree
                    .get_mut(child)
                    .expect("selected child has indegree");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(*child);
                }
            }
        }
    }
    if ordered.len() != selected.len() {
        return Err(RepositoryError::InvalidRepositoryState("dependency cycle").into());
    }
    Ok(ordered)
}

#[derive(Eq, PartialEq)]
struct SemanticView {
    tree: TreeId,
    // Conflict object boundaries and IDs are graph artifacts. The effective meaning is normalized
    // edit-keyed evidence, including the conflicted base tree: a matching failure against a
    // different target is not equivalent merely because its edit and path are unchanged.
    conflicts: BTreeMap<EditId, (FailedEditEvidence, String, TreeId)>,
}
async fn semantic_view<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    view: &ViewRecord,
) -> Result<SemanticView> {
    let mut normalized = BTreeMap::new();
    let mut c = ConflictSetStore::new(reader.objects)
        .cursor(view.unresolved_conflicts)
        .await?;
    while let Some(id) = c.next().await? {
        let conflict = reader.conflict(id).await?;
        let base_tree = reader.view(conflict.base_view).await?.tree;
        for (edit, evidence) in conflict.edits {
            let path = reader.edit(edit).await?.path;
            if normalized
                .insert(edit, (evidence, path, base_tree))
                .is_some()
            {
                return Err(RepositoryRewriteError::SemanticDrift);
            }
        }
    }
    Ok(SemanticView {
        tree: view.tree,
        conflicts: normalized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(index: usize) -> ChangeId {
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&(index as u64).to_be_bytes());
        ChangeId::from_bytes(bytes)
    }

    fn revision(index: usize, dependencies: BTreeSet<ChangeId>) -> (RevisionId, RevisionRecord) {
        let mut digest = [0_u8; 32];
        digest[..8].copy_from_slice(&(index as u64).to_be_bytes());
        (
            RevisionId::from_digest(digest),
            RevisionRecord {
                change_id: change(index),
                metadata: RevisionMetadata::default(),
                predecessor: None,
                combines: BTreeSet::new(),
                generation: 0,
                dependencies,
                authored_against: ViewId::from_digest([0; 32]),
                edits: BTreeSet::new(),
                transition: EditTransition::default(),
                resulting_tree: TreeId::from_digest([0; 32]),
            },
        )
    }

    #[test]
    fn iterative_graph_walk_handles_a_250k_deep_chain() {
        const COUNT: usize = 250_000;
        let mut records = BTreeMap::new();
        for index in 0..COUNT {
            let dependencies = (index != 0)
                .then(|| change(index - 1))
                .into_iter()
                .collect();
            records.insert(change(index), revision(index, dependencies));
        }
        let reverse = reverse_dependencies(&records).unwrap();
        let selected = collect_descendants([change(0)], &reverse);
        assert_eq!(selected.len(), COUNT);
        let ordered = topo(&records, &selected, &reverse).unwrap();
        assert_eq!(ordered.first(), Some(&change(0)));
        assert_eq!(ordered.last(), Some(&change(COUNT - 1)));
    }

    #[test]
    fn iterative_graph_walk_handles_a_250k_broad_dag() {
        const COUNT: usize = 250_000;
        let root = change(0);
        let mut records = BTreeMap::new();
        records.insert(root, revision(0, BTreeSet::new()));
        for index in 1..COUNT {
            records.insert(change(index), revision(index, [root].into()));
        }
        let reverse = reverse_dependencies(&records).unwrap();
        let selected = collect_descendants([root], &reverse);
        assert_eq!(selected.len(), COUNT);
        let ordered = topo(&records, &selected, &reverse).unwrap();
        assert_eq!(ordered.len(), COUNT);
        assert_eq!(ordered[0], root);
    }
}
