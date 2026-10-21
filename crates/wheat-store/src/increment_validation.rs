//! Validation of what a repository envelope adds to a trusted one, at the trust
//! boundaries (publish, fetch, clone), in work proportional to what was added.
//!
//! The old envelope is trusted: it passed this validation, or full validation, when it
//! arrived. Everything the new state adds is found from the differences of its
//! collections against the old state's, and every new object is checked against a
//! trusted or itself checked counterpart: a new view against its composition parent, a
//! new revision's tree against the view it was authored on, a new application's against
//! its target. Shared subtrees are skipped by ID because they belong to an object
//! already proven, never because they happen to be in the store. The checks are full
//! validation's ([`crate::semantic_checks`]), so the two prove the same invariants.
//! See `docs/design/wheat-incremental-validation.md`.

use std::collections::{BTreeSet, HashMap, HashSet};

use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, ChangeObjectId, ContentId, ObjectId,
    RepositoryEnvelopeId, RepositoryStateRecord, RevisionId, ViewId, ViewLabelManifestId,
    ViewRecord,
};

use crate::{
    ChangeMapDelta, ChangeMapDeltaLimits, ChangeMapStore, CollectionError, ConflictSetStore,
    ContentSetDelta, ContentSetDeltaLimits, ContentSetStore, ContentStoreError,
    ImmutableObjectStore, ProjectedCommitSetDelta, ProjectedCommitSetStore,
    RepositoryPublicationError, RepositoryReader, RetainedSetDelta, RetainedSetDeltaLimits,
    RetainedSetStore, ViewLineageMapStore, radix,
    semantic_checks::{Checks, Membership, Retained, invalid},
    verification_scratch::VerificationScratch,
};

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

/// What an increment added, as validated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IncrementReport {
    pub changes: usize,
    pub views: usize,
    pub revisions: usize,
    pub applications: usize,
    pub edges: usize,
    pub content: usize,
    /// Labels created or moved.
    pub labels: usize,
}

const UNBOUNDED_RETAINED: RetainedSetDeltaLimits = RetainedSetDeltaLimits {
    max_changes: usize::MAX,
    max_nodes: usize::MAX,
    max_pending: usize::MAX,
};

/// Membership in the new state. Objects the increment added are answered from memory,
/// everything else by a lookup in the new state's collections.
struct NewState<'a, O> {
    objects: &'a O,
    state: RepositoryStateRecord,
    changes: HashSet<ObjectId>,
    views: HashSet<ObjectId>,
    revisions: HashSet<ObjectId>,
    applications: HashSet<ObjectId>,
    content: HashSet<ContentId>,
}

impl<O: ImmutableObjectStore> Membership for NewState<'_, O> {
    async fn retained(&self, kind: Retained, id: ObjectId) -> Result<()> {
        let (added, root) = match kind {
            Retained::Changes => (&self.changes, self.state.retained_changes),
            Retained::Views => (&self.views, self.state.retained_views),
            Retained::Revisions => (&self.revisions, self.state.retained_revisions),
            Retained::Applications => (&self.applications, self.state.retained_applications),
        };
        if !added.contains(&id)
            && !RetainedSetStore::new(self.objects)
                .contains(root, id)
                .await?
        {
            return Err(invalid("reference not retained"));
        }
        Ok(())
    }

    async fn change(&self, change: ChangeId) -> Result<()> {
        if ChangeMapStore::new(self.objects)
            .get(self.state.changes, change)
            .await?
            .is_none()
        {
            return Err(invalid("logical change not retained"));
        }
        Ok(())
    }

    async fn content(&self, content: ContentId) -> Result<()> {
        if !self.content.contains(&content)
            && !ContentSetStore::new(self.objects)
                .contains(self.state.required_content, content)
                .await?
        {
            return Err(invalid("content absent from aggregate manifest"));
        }
        Ok(())
    }
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// Validates what envelope `new` adds to the trusted envelope `old`: its history only
    /// grows, and every change, revision, application, view, lineage edge, content
    /// member and label it adds keeps the store's invariants. The work follows what was
    /// added, not the size of the history.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn validate_increment(
        &self,
        old: RepositoryEnvelopeId,
        new: RepositoryEnvelopeId,
    ) -> Result<IncrementReport> {
        let old = self.envelope(old).await?;
        let new = self.envelope(new).await?;
        let old_state = self.state(old.state).await?;
        let new_state = self.state(new.state).await?;
        let mut report = IncrementReport::default();
        if old.state != new.state {
            report = self.validate_state_increment(&old_state, new_state).await?;
        }
        report.labels = self
            .validate_label_increment(old.labels, new.labels, &new_state)
            .await?;
        Ok(report)
    }

    async fn validate_state_increment(
        &self,
        old: &RepositoryStateRecord,
        new: RepositoryStateRecord,
    ) -> Result<IncrementReport> {
        // Step 1: the history only grows.
        if old.base_view != new.base_view {
            return Err(invalid("publication changes repository base view"));
        }
        let changes = self
            .added_retained(old.retained_changes, new.retained_changes)
            .await?;
        let views = self
            .added_retained(old.retained_views, new.retained_views)
            .await?;
        let revisions = self
            .added_retained(old.retained_revisions, new.retained_revisions)
            .await?;
        let applications = self
            .added_retained(old.retained_applications, new.retained_applications)
            .await?;
        let content = self.added_content(old, &new).await?;
        self.validate_projected_commits(old, &new).await?;
        let state = NewState {
            objects: self.objects,
            state: new,
            changes: changes.iter().copied().collect(),
            views: views.iter().copied().collect(),
            revisions: revisions.iter().copied().collect(),
            applications: applications.iter().copied().collect(),
            content: content.iter().copied().collect(),
        };
        let checks = Checks {
            reader: self,
            membership: &state,
        };

        self.validate_new_changes(old, &new, &state.changes).await?;
        let edges = self.new_lineage_edges(old, &new).await?;

        // Step 5: each new view is its composition parent plus one application (I1).
        // No order is needed: the result stands only once every check has passed.
        let mut records = HashMap::with_capacity(views.len());
        let mut composed = HashSet::with_capacity(views.len());
        let mut scratch = None;
        for id in &views {
            let id = ViewId::from_digest(*id.as_bytes());
            let view = self.view(id).await?;
            self.validate_new_view(&checks, id, &view, &edges, &mut composed, &mut scratch)
                .await?;
            records.insert(id, view);
        }

        // Step 6: every new edge joins retained views and increases the weight (I3),
        // so the lineage stays acyclic whatever valid state it is joined with.
        for (parent, child) in &edges {
            for view in [parent, child] {
                if !state.views.contains(&(*view).into())
                    && !RetainedSetStore::new(self.objects)
                        .contains(new.retained_views, (*view).into())
                        .await?
                {
                    return Err(invalid("lineage edge names an unretained view"));
                }
            }
            if self.weight(&records, *child).await? <= self.weight(&records, *parent).await? {
                return Err(invalid("lineage edge does not increase view weight"));
            }
        }

        // Step 3: revisions (I5).
        for id in &revisions {
            let revision = self
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            checks.revision(&revision).await?;
        }

        // Step 4: applications (I6, I2), each composing a new view.
        for id in &applications {
            let id = ApplicationId::from_digest(*id.as_bytes());
            let application = self.application(id).await?;
            let context = checks.application(&application).await?;
            checks
                .application_placement(id, &application, &context.revision, &context.target)
                .await?;
            if !composed.contains(&id) {
                return Err(invalid("new application composes no retained view"));
            }
        }

        Ok(IncrementReport {
            changes: changes.len(),
            views: views.len(),
            revisions: revisions.len(),
            applications: applications.len(),
            edges: edges.len(),
            content: content.len(),
            labels: 0,
        })
    }

    /// The members `new` adds to the retained set `old`, refusing any it drops.
    async fn added_retained(
        &self,
        old: wheat_core::RetainedSetId,
        new: wheat_core::RetainedSetId,
    ) -> Result<Vec<ObjectId>> {
        let mut added = Vec::new();
        for entry in RetainedSetStore::new(self.objects)
            .delta(old, new, UNBOUNDED_RETAINED)
            .await?
        {
            match entry {
                RetainedSetDelta::Added(id) => added.push(id),
                RetainedSetDelta::Removed(_) => {
                    return Err(invalid("publication drops retained history"));
                }
            }
        }
        Ok(added)
    }

    /// Step 7: the members the new aggregate content set adds, each present in the
    /// content store. Existence is read from stored metadata: content was checked
    /// against its ID when it was stored.
    async fn added_content(
        &self,
        old: &RepositoryStateRecord,
        new: &RepositoryStateRecord,
    ) -> Result<Vec<ContentId>> {
        let mut added = Vec::new();
        for entry in ContentSetStore::new(self.objects)
            .delta(
                old.required_content,
                new.required_content,
                ContentSetDeltaLimits {
                    max_changes: usize::MAX,
                    max_nodes: usize::MAX,
                    max_pending: usize::MAX,
                },
            )
            .await?
        {
            match entry {
                ContentSetDelta::Added(content) => {
                    match self.content.logical_length(content) {
                        Ok(_) => {}
                        Err(ContentStoreError::Missing(_)) => {
                            return Err(RepositoryPublicationError::MissingContent(content));
                        }
                        Err(error) => return Err(error.into()),
                    }
                    added.push(content);
                }
                ContentSetDelta::Removed(_) => {
                    return Err(invalid("publication drops retained content"));
                }
            }
        }
        Ok(added)
    }

    /// A new projected commit names a retained application. Members leave only with
    /// their applications, when history is rewritten.
    async fn validate_projected_commits(
        &self,
        old: &RepositoryStateRecord,
        new: &RepositoryStateRecord,
    ) -> Result<()> {
        for entry in ProjectedCommitSetStore::new(self.objects)
            .delta(
                old.projected_commits,
                new.projected_commits,
                UNBOUNDED_RETAINED,
            )
            .await?
        {
            match entry {
                ProjectedCommitSetDelta::Added(member) => {
                    if !RetainedSetStore::new(self.objects)
                        .contains(new.retained_applications, member.application().into())
                        .await?
                    {
                        return Err(invalid("projected commit names a non-retained application"));
                    }
                }
                ProjectedCommitSetDelta::Removed(_) => {
                    return Err(invalid("publication drops projected commits"));
                }
            }
        }
        Ok(())
    }

    /// Step 2 (I4): the change map gains exactly one entry per new change object, keyed
    /// by its logical ID, and keeps every entry it had.
    async fn validate_new_changes(
        &self,
        old: &RepositoryStateRecord,
        new: &RepositoryStateRecord,
        objects: &HashSet<ObjectId>,
    ) -> Result<()> {
        let entries = ChangeMapStore::new(self.objects)
            .delta(
                old.changes,
                new.changes,
                ChangeMapDeltaLimits {
                    max_changes: usize::MAX,
                    max_nodes: usize::MAX,
                    max_pending: usize::MAX,
                },
            )
            .await?;
        // Keys are distinct and each names a new object whose logical ID is that key,
        // so the entries name distinct new objects; equal counts make it a bijection.
        if entries.len() != objects.len() {
            return Err(invalid("change map differs from retained logical changes"));
        }
        for entry in entries {
            let (change, object): (ChangeId, ChangeObjectId) = match entry {
                ChangeMapDelta::Added { change, object } => (change, object),
                ChangeMapDelta::Changed { .. } | ChangeMapDelta::Removed { .. } => {
                    return Err(invalid("publication rewrites change-map entries"));
                }
            };
            if !objects.contains(&object.into()) {
                return Err(invalid("change map differs from retained logical changes"));
            }
            if self.change(object).await?.id != change {
                return Err(invalid("change-map key does not match logical change id"));
            }
        }
        Ok(())
    }

    /// The lineage edges `new` adds to `old`, refusing any it drops. Every parent it
    /// adds has children.
    async fn new_lineage_edges(
        &self,
        old: &RepositoryStateRecord,
        new: &RepositoryStateRecord,
    ) -> Result<BTreeSet<(ViewId, ViewId)>> {
        let lineage = ViewLineageMapStore::new(self.objects);
        let sets = RetainedSetStore::new(self.objects);
        let mut edges = BTreeSet::new();
        for entry in lineage
            .parent_delta(old.view_lineage, new.view_lineage)
            .await
            .map_err(|_| invalid("invalid view lineage"))?
        {
            match entry {
                radix::DiffEntry::Added(parent, children) => {
                    let mut cursor = sets.cursor(children).await?;
                    let before = edges.len();
                    while let Some(child) = cursor.next().await? {
                        edges.insert((parent, ViewId::from_digest(*child.as_bytes())));
                    }
                    if edges.len() == before {
                        return Err(invalid("view lineage names a parent without children"));
                    }
                }
                radix::DiffEntry::Changed(parent, old_children, new_children) => {
                    for entry in sets
                        .delta(old_children, new_children, UNBOUNDED_RETAINED)
                        .await?
                    {
                        match entry {
                            RetainedSetDelta::Added(child) => {
                                edges.insert((parent, ViewId::from_digest(*child.as_bytes())));
                            }
                            RetainedSetDelta::Removed(_) => {
                                return Err(invalid("publication drops view lineage"));
                            }
                        }
                    }
                }
                radix::DiffEntry::Removed(..) => {
                    return Err(invalid("publication drops view lineage"));
                }
            }
        }
        Ok(edges)
    }

    /// Step 5 for one new view: its composition, an edge from its composition parent,
    /// its last application retained, its conflicts its parent's plus that
    /// application's, and its content map its parent's adjusted by the tree diff.
    async fn validate_new_view(
        &self,
        checks: &Checks<'_, '_, O, NewState<'_, O>>,
        id: ViewId,
        view: &ViewRecord,
        edges: &BTreeSet<(ViewId, ViewId)>,
        composed: &mut HashSet<ApplicationId>,
        scratch: &mut Option<VerificationScratch>,
    ) -> Result<()> {
        let composition = checks
            .composition(view, |parent| edges.contains(&(parent, id)))
            .await?;
        checks
            .membership
            .retained(Retained::Applications, composition.application_id.into())
            .await?;
        let produced = match composition.application.result {
            ApplicationResultRecord::Clean { .. } => None,
            ApplicationResultRecord::Conflicted { conflict, .. } => Some(conflict),
        };
        let conflicts = match ConflictSetStore::new(self.objects)
            .delta(
                composition.parent.unresolved_conflicts,
                view.unresolved_conflicts,
                1,
            )
            .await
        {
            Err(CollectionError::DeltaChangeLimitExceeded { .. }) => None,
            delta => Some(delta?),
        };
        let expected = produced.map(|conflict| radix::DiffEntry::Added(conflict, ()));
        if conflicts.as_deref() != Some(expected.as_slice()) {
            return Err(invalid(
                "view conflicts are not its composition parent's plus its last application's",
            ));
        }
        if !checks
            .content_map_follows_tree_diff(&composition.parent, view)
            .await?
        {
            let scratch = match scratch {
                Some(scratch) => scratch,
                None => scratch.insert(VerificationScratch::new()?),
            };
            checks
                .content_map_matches_tree(scratch, view.tree, view.required_content)
                .await?;
        }
        composed.insert(composition.application_id);
        Ok(())
    }

    /// The weight of `view`: from the new views read, or from the store.
    async fn weight(&self, records: &HashMap<ViewId, ViewRecord>, view: ViewId) -> Result<u64> {
        Ok(match records.get(&view) {
            Some(record) => record.order.weight,
            None => self.view(view).await?.order.weight,
        })
    }

    /// Step 8 (I8): every label the new manifest creates or moves names a view the new
    /// state retains. Labels it keeps named views the old state retained, which the new
    /// one still does. Decoding enforces the label cap.
    async fn validate_label_increment(
        &self,
        old: ViewLabelManifestId,
        new: ViewLabelManifestId,
        state: &RepositoryStateRecord,
    ) -> Result<usize> {
        if old == new {
            return Ok(0);
        }
        let old = self.view_label_manifest(old).await?;
        let new = self.view_label_manifest(new).await?;
        let mut changed = 0;
        for (name, label) in &new.labels {
            if old.labels.get(name).map(|old| old.view) == Some(label.view) {
                continue;
            }
            if !RetainedSetStore::new(self.objects)
                .contains(state.retained_views, label.view.into())
                .await?
            {
                return Err(invalid("view label target not retained"));
            }
            self.view(label.view).await?;
            changed += 1;
        }
        Ok(changed)
    }
}
