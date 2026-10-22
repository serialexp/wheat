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

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;

use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, ChangeObjectId, ContentId, ObjectId,
    RepositoryEnvelopeId, RepositoryStateRecord, RevisionId, ViewId, ViewLabelManifestId,
    ViewLabelManifestRecord, ViewRecord,
};

use crate::{
    ChangeMapDelta, ChangeMapDeltaLimits, ChangeMapStore, CollectionError, ConflictSetStore,
    ContentSetDelta, ContentSetDeltaLimits, ContentSetStore, ContentStoreError,
    ImmutableObjectStore, ObjectStoreError, ProjectedCommitSetDelta, ProjectedCommitSetStore,
    RepositoryPublicationError, RepositoryReader, RetainedSetDelta, RetainedSetDeltaLimits,
    RetainedSetStore, StoredObject, ViewLineageMapStore, radix,
    repository_verify::CachedObjects,
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
    /// The units the budget was sized from (see [`IncrementBudget`]), the object reads
    /// it allowed, and the reads checking the increment took. Zero when unmetered.
    pub budget_units: u64,
    pub budget_allowance: u64,
    pub budget_spent: u64,
}

/// What an envelope arriving at a trust boundary (publish, fetch, clone) is validated
/// against, which decides how [`RepositoryReader::validate_arrival`] checks it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Arrival {
    /// An ordinary publish or fetch: the new envelope extends this trusted one, and
    /// only what it adds is checked, within [`IncrementBudget::STANDARD`].
    Extends(RepositoryEnvelopeId),
    /// A clone, or a fetch into a store with no trusted envelope of the remote:
    /// everything is new and checked as an increment from the base view.
    Bootstrap,
    /// A forced publish or fetch, which may remove history, so nothing unchanged can be
    /// trusted to still hold: full validation and every label.
    Forced,
}

/// How many object reads checking an increment may take, relative to what it adds.
///
/// A unit is one object the increment adds (change, view, revision, application,
/// lineage edge, content member) or one edit that a new revision, or a new
/// application's revision, replays. Each unit may take `objects_per_unit` reads per bit
/// of the history's size (the largest new view's change count), since lookups in the
/// history's collections deepen with its logarithm. An application replaying a large
/// old revision is paid for by that revision's edits; a small increment can't make
/// validation read much of the history. Finding what was added is not metered: it
/// reads in proportion to what was added.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IncrementBudget {
    pub objects_per_unit: u64,
}

impl IncrementBudget {
    /// The budget for increments from other processes: about eight times what a
    /// one-change publication reads (82, 86 and 110 reads for its 8 units after 1,000,
    /// 10,000 and 100,000 changes, near one per unit and bit; bench
    /// `increment_validate`).
    pub const STANDARD: Self = Self {
        objects_per_unit: 8,
    };

    /// History sizes below this many bits are treated as this many: a small history's
    /// collections are shallow, but not shallower than a few levels.
    const MIN_DEPTH: u64 = 8;

    fn allowance(self, units: u64, history_size: u64) -> u64 {
        let depth = u64::from(u64::BITS - history_size.leading_zeros()).max(Self::MIN_DEPTH);
        self.objects_per_unit
            .saturating_mul(units)
            .saturating_mul(depth)
    }
}

/// What a state adds to a trusted one, found from the differences of their collections.
struct StateDelta {
    changes: HashSet<ObjectId>,
    /// The new views' records, read while finding the delta.
    views: HashMap<ViewId, ViewRecord>,
    revisions: Vec<ObjectId>,
    applications: Vec<ObjectId>,
    content: Vec<ContentId>,
    edges: BTreeSet<(ViewId, ViewId)>,
}

impl StateDelta {
    /// The most changes a new view selects: the history's size, for the budget.
    fn history_size(&self) -> u64 {
        self.views
            .values()
            .map(|view| view.order.size)
            .max()
            .unwrap_or(0)
    }
}

/// An object store refusing reads past an allowance. Once refused, the validation fails
/// and its caller reports the budget, not the read error.
struct Metered<'a, O> {
    inner: &'a O,
    allowance: u64,
    spent: AtomicU64,
}

impl<'a, O> Metered<'a, O> {
    fn new(inner: &'a O, allowance: u64) -> Self {
        Self {
            inner,
            allowance,
            spent: AtomicU64::new(0),
        }
    }

    /// Reads taken, counting any refused.
    fn spent(&self) -> u64 {
        self.spent.load(Ordering::Relaxed)
    }

    fn exceeded(&self) -> bool {
        self.spent() > self.allowance
    }
}

#[async_trait]
impl<O: ImmutableObjectStore> ImmutableObjectStore for Metered<'_, O> {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        bytes: &[u8],
    ) -> std::result::Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }

    async fn get(&self, id: ObjectId) -> std::result::Result<StoredObject, ObjectStoreError> {
        if self.spent.fetch_add(1, Ordering::Relaxed) >= self.allowance {
            return Err(ObjectStoreError::Backend(
                "increment validation budget exceeded".into(),
            ));
        }
        self.inner.get(id).await
    }

    async fn exists_many(
        &self,
        ids: &[ObjectId],
    ) -> std::result::Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
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
    /// added, not the size of the history, and an increment needing more reads than
    /// `budget` allows for what it adds is refused.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn validate_increment(
        &self,
        old: RepositoryEnvelopeId,
        new: RepositoryEnvelopeId,
        budget: IncrementBudget,
    ) -> Result<IncrementReport> {
        let old = self.envelope(old).await?;
        let new = self.envelope(new).await?;
        let old_state = self.state(old.state).await?;
        let new_state = self.state(new.state).await?;
        let mut report = IncrementReport::default();
        if old.state != new.state {
            report = self
                .validate_state_increment(&old_state, new_state, Some(budget))
                .await?;
        }
        report.labels = self
            .validate_label_increment(old.labels, new.labels, &new_state)
            .await?;
        Ok(report)
    }

    /// Validates envelope `new` with no trusted envelope before it, as a clone does:
    /// the increment from a state holding only `new`'s base view, so every other
    /// retained object is new and checked by the same steps as an ordinary increment.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn validate_bootstrap(&self, new: RepositoryEnvelopeId) -> Result<IncrementReport> {
        // Every object is new, so radix nodes are read many times over, as in full
        // validation; read through the same per-run cache.
        let cached = CachedObjects::for_validation(self.objects);
        RepositoryReader::new(&cached, self.content)
            .bootstrap(new)
            .await
    }

    /// Validates envelope `new` as `arrival` says: see [`Arrival`].
    pub async fn validate_arrival(
        &self,
        new: RepositoryEnvelopeId,
        arrival: Arrival,
    ) -> Result<()> {
        match arrival {
            Arrival::Extends(old) => {
                self.validate_increment(old, new, IncrementBudget::STANDARD)
                    .await?;
            }
            Arrival::Bootstrap => {
                self.validate_bootstrap(new).await?;
            }
            Arrival::Forced => {
                self.validate_envelope(new).await?;
            }
        }
        Ok(())
    }

    async fn bootstrap(&self, new: RepositoryEnvelopeId) -> Result<IncrementReport> {
        let new = self.envelope(new).await?;
        let new_state = self.state(new.state).await?;
        let sets = RetainedSetStore::new(self.objects);
        let empty_set = sets.empty_root().await?;
        let empty = RepositoryStateRecord {
            base_view: new_state.base_view,
            changes: ChangeMapStore::new(self.objects).empty_root().await?,
            view_lineage: ViewLineageMapStore::new(self.objects)
                .empty_root()
                .await
                .map_err(|_| invalid("invalid view lineage"))?,
            required_content: ContentSetStore::new(self.objects).empty_root().await?,
            retained_changes: empty_set,
            retained_views: empty_set,
            retained_revisions: empty_set,
            retained_applications: empty_set,
            projected_commits: ProjectedCommitSetStore::new(self.objects)
                .empty_root()
                .await?,
        };
        if !sets
            .contains(new_state.retained_views, new_state.base_view.into())
            .await?
        {
            return Err(invalid("base view not retained"));
        }
        let mut report = self
            .validate_state_increment(&empty, new_state, None)
            .await?;
        let no_labels = ViewLabelManifestRecord {
            labels: BTreeMap::new(),
        };
        report.labels = self
            .validate_labels_against(&no_labels, new.labels, &new_state)
            .await?;
        Ok(report)
    }

    /// Finds what `new` adds to `old`, then checks it. Finding it reads in proportion
    /// to what was added and is not metered; with a `budget`, checking it reads through
    /// a [`Metered`] store sized from what was found. Without one (bootstrapping) the
    /// check is unmetered.
    async fn validate_state_increment(
        &self,
        old: &RepositoryStateRecord,
        new: RepositoryStateRecord,
        budget: Option<IncrementBudget>,
    ) -> Result<IncrementReport> {
        let delta = self.state_delta(old, &new).await?;
        let mut report = IncrementReport {
            changes: delta.changes.len(),
            views: delta.views.len(),
            revisions: delta.revisions.len(),
            applications: delta.applications.len(),
            edges: delta.edges.len(),
            content: delta.content.len(),
            ..IncrementReport::default()
        };
        let Some(budget) = budget else {
            self.check_state_delta(&new, &delta).await?;
            self.check_combinations(&delta).await?;
            return Ok(report);
        };
        let units = self.budget_units(&delta).await?;
        let allowance = budget.allowance(units, delta.history_size());
        let metered = Metered::new(self.objects, allowance);
        let checked = RepositoryReader::new(&metered, self.content)
            .check_state_delta(&new, &delta)
            .await;
        report.budget_units = units;
        report.budget_allowance = allowance;
        report.budget_spent = metered.spent();
        if metered.exceeded() {
            return Err(RepositoryPublicationError::OverBudget { allowance });
        }
        checked?;
        self.check_combinations(&delta).await?;
        Ok(report)
    }

    /// Checks each new combined revision is the merge of its parents. A merge may read
    /// back to a distant base, which what the increment adds says nothing about, so it
    /// is bounded per revision by a [`crate::MergeBudget`] instead of by the increment's
    /// allowance.
    async fn check_combinations(&self, delta: &StateDelta) -> Result<()> {
        for id in &delta.revisions {
            let revision = self
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            self.check_combination(&revision).await?;
        }
        Ok(())
    }

    /// The units an increment's budget is sized from: one per object it adds, and one
    /// per edit each new revision and each new application's revision replays.
    async fn budget_units(&self, delta: &StateDelta) -> Result<u64> {
        let objects = delta.changes.len()
            + delta.views.len()
            + delta.revisions.len()
            + delta.applications.len()
            + delta.edges.len()
            + delta.content.len();
        let mut units = u64::try_from(objects).unwrap_or(u64::MAX);
        let mut edits = |revision: &wheat_core::RevisionRecord| {
            units = units.saturating_add(u64::try_from(revision.edits.len()).unwrap_or(u64::MAX));
        };
        for id in &delta.revisions {
            edits(
                &self
                    .revision(RevisionId::from_digest(*id.as_bytes()))
                    .await?,
            );
        }
        for id in &delta.applications {
            let application = self
                .application(ApplicationId::from_digest(*id.as_bytes()))
                .await?;
            edits(&self.revision(application.revision_id).await?);
        }
        Ok(units)
    }

    /// What `new` adds to `old`: the retained objects, aggregate content and lineage
    /// edges, with the history only growing (step 1), the change map gaining exactly the
    /// new changes (step 2) and new projected commits naming retained applications.
    async fn state_delta(
        &self,
        old: &RepositoryStateRecord,
        new: &RepositoryStateRecord,
    ) -> Result<StateDelta> {
        if old.base_view != new.base_view {
            return Err(invalid("publication changes repository base view"));
        }
        let changes: HashSet<ObjectId> = self
            .added_retained(old.retained_changes, new.retained_changes)
            .await?
            .into_iter()
            .collect();
        let views = self
            .added_retained(old.retained_views, new.retained_views)
            .await?;
        let revisions = self
            .added_retained(old.retained_revisions, new.retained_revisions)
            .await?;
        let applications = self
            .added_retained(old.retained_applications, new.retained_applications)
            .await?;
        let content = self.added_content(old, new).await?;
        self.validate_projected_commits(old, new).await?;
        self.validate_new_changes(old, new, &changes).await?;
        let edges = self.new_lineage_edges(old, new).await?;
        let mut records = HashMap::with_capacity(views.len());
        for id in &views {
            let id = ViewId::from_digest(*id.as_bytes());
            records.insert(id, self.view(id).await?);
        }
        Ok(StateDelta {
            changes,
            views: records,
            revisions,
            applications,
            content,
            edges,
        })
    }

    /// Checks every view, lineage edge, revision and application the increment adds,
    /// against the new state `new` (steps 3 to 6).
    async fn check_state_delta(
        &self,
        new: &RepositoryStateRecord,
        delta: &StateDelta,
    ) -> Result<()> {
        let state = NewState {
            objects: self.objects,
            state: *new,
            changes: delta.changes.clone(),
            views: delta.views.keys().map(|&id| id.into()).collect(),
            revisions: delta.revisions.iter().copied().collect(),
            applications: delta.applications.iter().copied().collect(),
            content: delta.content.iter().copied().collect(),
        };
        let checks = Checks {
            reader: self,
            membership: &state,
        };
        let records = &delta.views;
        let edges = &delta.edges;

        // Step 5: each new view is its composition parent plus one application (I1).
        // No order is needed: the result stands only once every check has passed.
        let mut composed = HashSet::with_capacity(records.len());
        let mut scratch = None;
        for (&id, view) in records {
            if id == new.base_view {
                // New only when bootstrapping: it selects nothing (I1's base rule).
                checks.base(view).await?;
                continue;
            }
            self.validate_new_view(&checks, id, view, edges, &mut composed, &mut scratch)
                .await?;
        }

        // Step 6: every new edge joins retained views and increases the weight (I3),
        // so the lineage stays acyclic whatever valid state it is joined with.
        for (parent, child) in edges {
            for view in [parent, child] {
                if !state.views.contains(&(*view).into())
                    && !RetainedSetStore::new(self.objects)
                        .contains(new.retained_views, (*view).into())
                        .await?
                {
                    return Err(invalid("lineage edge names an unretained view"));
                }
            }
            if self.weight(records, *child).await? <= self.weight(records, *parent).await? {
                return Err(invalid("lineage edge does not increase view weight"));
            }
        }

        // Step 3: revisions (I5).
        for id in &delta.revisions {
            let revision = self
                .revision(RevisionId::from_digest(*id.as_bytes()))
                .await?;
            checks.revision(&revision).await?;
        }

        // Step 4: applications (I6, I2), each composing a new view.
        for id in &delta.applications {
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
        Ok(())
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
        self.validate_labels_against(&old, new, state).await
    }

    /// The labels of manifest `new` that are absent from or moved since `old`, each
    /// checked to name a view `state` retains.
    async fn validate_labels_against(
        &self,
        old: &ViewLabelManifestRecord,
        new: ViewLabelManifestId,
        state: &RepositoryStateRecord,
    ) -> Result<usize> {
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
