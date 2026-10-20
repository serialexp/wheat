//! The structural objects one repository envelope has that another does not.
//!
//! [`StructuralDelta`] walks the closure of a `new` envelope and yields every object in
//! it, except whole subgraphs it can prove are also in the closure of an `old`
//! envelope. It is used on both ends of a fetch: the server streams what it yields,
//! and the client runs the same walk over its own store afterwards to check that it
//! has everything, asking for whatever is still missing.
//!
//! What each object refers to comes from [`crate::object_graph`], the same list a
//! snapshot export follows. This module only decides how to pair each reference with
//! the old side.
//!
//! # What may be skipped
//!
//! Objects are content-addressed, so an object's ID fixes everything below it. The walk
//! skips an object only when one of these holds:
//!
//! 1. it has been yielded already in this walk (its references were followed then);
//! 2. an object of the old closure refers to the same ID at the matching position: the
//!    same state field, the same directory entry name, the same radix key path, the
//!    same field of the old view it is paired with;
//! 3. it is a member of the matching retained set of the old state (views, revisions,
//!    applications, changes), or an edit of a revision that is.
//!
//! Every other object is read, yielded, and has all of its references followed. That
//! makes the walk sound whatever the pairing: pairing only decides how much can be
//! skipped. Pairing misses little: a collection that became empty sends its one-node
//! empty root again, and a subgraph that moved to a place with no old counterpart is
//! sent again. It needs the old closure to be complete where it is read; an old object
//! that is missing counts as not there, which only makes the walk skip less.
//!
//! # Pairing
//!
//! The state and envelope pair field by field. Radix nodes pair by key path (with path
//! compression, an old node may cover only part of a new node's prefix). Directory
//! nodes pair by entry name. A new view pairs with its nearest ancestor in the old
//! retained views, found through the lineage map's new parent → child links; a
//! revision's tree pairs with the tree of the view it was authored against, an
//! application's with its target view's. Views are only walked after the state-level
//! structures, so every new lineage link is known by then.
//!
//! # Missing objects
//!
//! An object of the new side that is not in the store parks its task. When nothing else
//! can run, [`StructuralDelta::next`] reports the parked IDs as
//! [`DeltaStep::Missing`]; the caller stores them (or some of them) and calls
//! [`StructuralDelta::resume`].

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use thiserror::Error;
use wheat_core::{
    CanonicalObjectError, ChangeId, DirectoryEntry, ObjectId, RepositoryEnvelopeId, RetainedSetId,
    RevisionId, ViewId,
};

use crate::{
    ImmutableObjectStore, ObjectKind, ObjectStoreError, RetainedSetStore, StoredObject,
    object_graph::{
        self, ApplicationSite, ChangeSite, ConflictSite, EditSite, EnvelopeSite, LabelSite,
        MalformedRadix, RadixKind, RadixNode, RadixSite, Ref, RevisionSite, StateSite, TreeSite,
        ViewSite,
    },
    sets::CollectionError,
};

/// Bounds on one walk.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StructuralDeltaLimits {
    /// Most IDs reported in one [`DeltaStep::Missing`].
    pub max_missing_batch: usize,
    /// Most tasks waiting at once (runnable and parked).
    pub max_pending: usize,
    /// Most lineage links followed to find a new view's nearest old ancestor.
    pub max_lineage_chain: usize,
    /// Most old children of one view read to find the old view a new one replaces;
    /// a view with more pairs its new children with itself.
    pub max_old_children: usize,
}

impl Default for StructuralDeltaLimits {
    fn default() -> Self {
        Self {
            max_missing_batch: 4_096,
            max_pending: 4_000_000,
            max_lineage_chain: 100_000,
            max_old_children: 64,
        }
    }
}

/// One step of a [`StructuralDelta`].
#[derive(Debug)]
pub enum DeltaStep {
    /// An object of the new closure that is not proven to be in the old one.
    Object(StoredObject),
    /// Objects the walk needs and the store does not have. Store them, then call
    /// [`StructuralDelta::resume`].
    Missing(Vec<ObjectId>),
}

/// What one walk did, for reports and budgets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StructuralDeltaStats {
    /// Objects yielded.
    pub emitted: u64,
    /// Canonical bytes of the objects yielded.
    pub emitted_bytes: u64,
    /// Reads of new-side objects.
    pub new_reads: u64,
    /// Reads of old-side objects, for pairing.
    pub old_reads: u64,
    /// Old-side objects that were not in the store.
    pub old_missing: u64,
    /// Subgraphs skipped because the old side has the same ID at the same place.
    pub pruned_aligned: u64,
    /// Subgraphs skipped because the old side had the same ID at another place.
    pub pruned_proven_old: u64,
    /// Subgraphs skipped because the old state retains them.
    pub pruned_retained: u64,
    /// References skipped because they were yielded already.
    pub pruned_emitted: u64,
    /// Lookups in the old retained sets (cached ones not counted).
    pub retained_lookups: u64,
    /// Tasks parked on a missing object.
    pub parked: u64,
}

#[derive(Debug, Error)]
pub enum StructuralDeltaError {
    #[error(transparent)]
    Store(#[from] ObjectStoreError),
    #[error("object {id} does not decode")]
    Decode {
        id: ObjectId,
        #[source]
        source: CanonicalObjectError,
    },
    #[error("object {id} is a {actual:?} where a {expected} belongs")]
    WrongKind {
        id: ObjectId,
        expected: &'static str,
        actual: ObjectKind,
    },
    #[error("radix node {id} is malformed at key byte {position}")]
    MalformedRadix { id: ObjectId, position: usize },
    #[error("old-side collection lookup failed")]
    Collection(#[from] CollectionError),
    #[error("old-side view-lineage lookup failed")]
    OldLineage(#[source] crate::ViewLineageError),
    #[error("structural delta limit exceeded: {0}")]
    LimitExceeded(&'static str),
}

impl From<MalformedRadix> for StructuralDeltaError {
    fn from(MalformedRadix { id, position }: MalformedRadix) -> Self {
        Self::MalformedRadix { id, position }
    }
}

type Result<T> = std::result::Result<T, StructuralDeltaError>;

/// Which radix collection a node belongs to, and what its keys and values mean.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Role {
    RetainedViews,
    RetainedRevisions,
    RetainedApplications,
    RetainedChanges,
    /// Parent view → retained set of its children.
    Lineage,
    /// The children of one parent view.
    LineageChildren,
    ChangeMap,
    RevisionMap,
    ApplicationMap,
    ConflictSet,
    /// A view's content map.
    ViewContent,
    /// The state's aggregate content set.
    AggregateContent,
}

impl Role {
    fn radix_kind(self) -> RadixKind {
        match self {
            Self::RetainedViews
            | Self::RetainedRevisions
            | Self::RetainedApplications
            | Self::RetainedChanges
            | Self::LineageChildren => RadixKind::RetainedSet,
            Self::Lineage => RadixKind::ViewLineage,
            Self::ChangeMap => RadixKind::ChangeMap,
            Self::RevisionMap => RadixKind::RevisionMap,
            Self::ApplicationMap => RadixKind::ApplicationMap,
            Self::ConflictSet => RadixKind::ConflictSet,
            Self::ViewContent => RadixKind::ContentMap,
            Self::AggregateContent => RadixKind::ContentSet,
        }
    }

    fn kind(self) -> ObjectKind {
        self.radix_kind().object_kind()
    }

    /// Radix structures hanging off the state are walked before any view.
    fn state_level(self) -> bool {
        !matches!(
            self,
            Self::RevisionMap | Self::ApplicationMap | Self::ConflictSet | Self::ViewContent
        )
    }

    fn decode(self, bytes: &[u8]) -> std::result::Result<RadixNode, CanonicalObjectError> {
        self.radix_kind().decode(bytes)
    }
}

/// The key bytes above a radix node.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct KeyPath {
    len: u8,
    bytes: [u8; 32],
}

impl KeyPath {
    const ROOT: Self = Self {
        len: 0,
        bytes: [0; 32],
    };

    fn extended(&self, tail: &[u8]) -> Self {
        let mut next = *self;
        let start = usize::from(self.len);
        next.bytes[start..start + tail.len()].copy_from_slice(tail);
        next.len += tail.len() as u8;
        next
    }

    fn full(&self) -> [u8; 32] {
        self.bytes
    }
}

/// The old side of a radix node being walked.
///
/// Every variant describes all old keys below one key path, so "not listed" means
/// "not there", except for [`OldRadix::Unknown`].
#[derive(Clone)]
enum OldRadix {
    /// Nothing is known (no old collection, or an old node is missing).
    Unknown,
    /// An old node at the same key path, not read yet.
    Id(ObjectId),
    /// An old node whose remaining prefix, from `skip` on, starts at the same key path.
    At(Arc<RadixNode>, usize),
    /// The old keys below the key path, as suffixes, with their values. Empty when
    /// the old collection has nothing there.
    Keys(Arc<[(Vec<u8>, Option<ObjectId>)]>),
}

impl OldRadix {
    fn absent() -> Self {
        Self::Keys(Arc::from([]))
    }
}

/// The keys among `keys` that start with `bytes`, with `bytes` taken off.
fn keys_below<'k>(
    keys: impl IntoIterator<Item = &'k (Vec<u8>, Option<ObjectId>)>,
    bytes: &[u8],
) -> OldRadix {
    OldRadix::Keys(
        keys.into_iter()
            .filter_map(|(suffix, value)| {
                suffix
                    .strip_prefix(bytes)
                    .map(|rest| (rest.to_vec(), *value))
            })
            .collect(),
    )
}

/// The old value of the key that ends where `old` is.
fn old_value_at(old: &OldRadix) -> OldValue {
    match old {
        OldRadix::Unknown | OldRadix::Id(_) => OldValue::Unknown,
        OldRadix::At(node, skip) => match node.value {
            Some(value) if *skip == node.prefix.len() => OldValue::Present(value),
            _ => OldValue::Absent,
        },
        OldRadix::Keys(keys) => keys
            .iter()
            .find(|(suffix, _)| suffix.is_empty())
            .map_or(OldValue::Absent, |(_, value)| OldValue::Present(*value)),
    }
}

/// What is known about a key on the old side.
#[derive(Clone, Copy)]
enum OldValue {
    Absent,
    Present(Option<ObjectId>),
    Unknown,
}

/// An object that is referenced as an entity rather than as part of a structure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Entity {
    View,
    Revision,
    Application,
    Change,
    Conflict,
    Edit,
}

impl Entity {
    fn kind(self) -> ObjectKind {
        match self {
            Self::View => ObjectKind::View,
            Self::Revision => ObjectKind::Revision,
            Self::Application => ObjectKind::Application,
            Self::Change => ObjectKind::Change,
            Self::Conflict => ObjectKind::Conflict,
            Self::Edit => ObjectKind::Edit,
        }
    }

    /// The old retained set that proves this entity old, as an index into
    /// [`OldState::retained`].
    fn retained_index(self) -> Option<usize> {
        match self {
            Self::View => Some(0),
            Self::Revision => Some(1),
            Self::Application => Some(2),
            Self::Change => Some(3),
            Self::Conflict | Self::Edit => None,
        }
    }
}

#[derive(Clone)]
enum Task {
    Envelope {
        id: ObjectId,
        old: Option<ObjectId>,
    },
    State {
        id: ObjectId,
        old: Option<ObjectId>,
    },
    Labels {
        id: ObjectId,
    },
    Tree {
        id: ObjectId,
        old: Option<ObjectId>,
    },
    Radix {
        id: ObjectId,
        role: Role,
        path: KeyPath,
        old: OldRadix,
        /// For [`Role::LineageChildren`]: whose children these are.
        parent: Option<ViewId>,
    },
    Entity {
        id: ObjectId,
        entity: Entity,
        /// Already shown not to be retained by the old state.
        known_new: bool,
    },
}

impl Task {
    fn id(&self) -> ObjectId {
        match self {
            Self::Envelope { id, .. }
            | Self::State { id, .. }
            | Self::Labels { id }
            | Self::Tree { id, .. }
            | Self::Radix { id, .. }
            | Self::Entity { id, .. } => *id,
        }
    }

    fn stage(&self) -> Stage {
        let state_level = match self {
            Self::Envelope { .. } | Self::State { .. } | Self::Labels { .. } => true,
            Self::Radix { role, .. } => role.state_level(),
            Self::Tree { .. } => false,
            Self::Entity { entity, .. } => return Stage::for_entity(*entity),
        };
        if state_level {
            Stage::State
        } else {
            Stage::Entities
        }
    }
}

/// Tasks run stage by stage: a stage starts only when every earlier one is done,
/// including anything parked in it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Stage {
    /// The state and what hangs off it, so every new lineage link is known before a
    /// view is paired.
    State,
    Entities,
    /// Edits refer to nothing, and by now every old revision met has said which
    /// edits are old.
    Edits,
}

impl Stage {
    const ALL: [Self; 3] = [Self::State, Self::Entities, Self::Edits];

    fn for_entity(entity: Entity) -> Self {
        if entity == Entity::Edit {
            Self::Edits
        } else {
            Self::Entities
        }
    }
}

/// The parts of the old state the walk looks things up in.
struct OldState {
    /// Views, revisions, applications, changes.
    retained: [RetainedSetId; 4],
    lineage: wheat_core::ViewLineageMapId,
}

/// Walks the structural objects of `new` that `old` does not prove present.
pub struct StructuralDelta<'a, S> {
    objects: &'a S,
    limits: StructuralDeltaLimits,
    /// Runnable tasks, one stack per [`Stage`].
    stacks: [Vec<Task>; 3],
    parked: Vec<Task>,
    emitted: HashSet<ObjectId>,
    /// Objects pruned because the old side had the same ID at the same place. They
    /// are in the old closure wherever else they turn up: one radix node can sit in
    /// two collections, and its pair there may have a different shape.
    proven_old: HashSet<ObjectId>,
    radix_walked: HashSet<(ObjectId, Role, KeyPath)>,
    old_state: Option<OldState>,
    retained_cache: HashMap<(usize, ObjectId), bool>,
    /// New view → the old view it pairs with.
    counterparts: HashMap<ViewId, Option<ViewId>>,
    old_views: HashMap<ViewId, Option<wheat_core::ViewRecord>>,
    /// New view → the last change in its order; `None` for the base view or when
    /// the view is not here yet.
    new_last: HashMap<ViewId, Option<ChangeId>>,
    /// Old view → its old children by their last change; `None` when it has more than
    /// `max_old_children` or they can't be read.
    old_children: HashMap<ViewId, Option<HashMap<ChangeId, ViewId>>>,
    /// Edits of old retained revisions, and which revisions have been looked at.
    old_edits: HashSet<ObjectId>,
    old_revisions_learned: HashSet<RevisionId>,
    /// New lineage links, child → parent.
    parent_of: HashMap<ViewId, ViewId>,
    /// Reused for the references of each radix node.
    radix_references: Vec<Ref<RadixSite>>,
    stats: StructuralDeltaStats,
}

impl<'a, S: ImmutableObjectStore> StructuralDelta<'a, S> {
    /// Walks `new`, skipping what `old` proves present. With `old == Some(new)` there
    /// is nothing to walk.
    pub fn new(
        objects: &'a S,
        old: Option<RepositoryEnvelopeId>,
        new: RepositoryEnvelopeId,
        limits: StructuralDeltaLimits,
    ) -> Self {
        let mut walk = Self {
            objects,
            limits,
            stacks: Default::default(),
            parked: Vec::new(),
            emitted: HashSet::new(),
            proven_old: HashSet::new(),
            radix_walked: HashSet::new(),
            old_state: None,
            retained_cache: HashMap::new(),
            counterparts: HashMap::new(),
            old_views: HashMap::new(),
            new_last: HashMap::new(),
            old_children: HashMap::new(),
            old_edits: HashSet::new(),
            old_revisions_learned: HashSet::new(),
            parent_of: HashMap::new(),
            radix_references: Vec::new(),
            stats: StructuralDeltaStats::default(),
        };
        if old != Some(new) {
            walk.stacks[Stage::State as usize].push(Task::Envelope {
                id: new.into(),
                old: old.map(Into::into),
            });
        }
        walk
    }

    #[must_use]
    pub fn stats(&self) -> &StructuralDeltaStats {
        &self.stats
    }

    /// Makes every parked task runnable again. Tasks whose object is still missing
    /// park again and are reported again.
    pub fn resume(&mut self) {
        for task in std::mem::take(&mut self.parked) {
            self.stacks[task.stage() as usize].push(task);
        }
    }

    /// The next object to send, a request for missing objects, or `None` when the walk
    /// is complete.
    #[tracing::instrument(name = "structural_delta_next", level = "trace", skip_all)]
    pub async fn next(&mut self) -> Result<Option<DeltaStep>> {
        loop {
            let Some(task) = self.next_task() else {
                if self.parked.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(DeltaStep::Missing(self.missing_ids())));
            };
            if let Some(object) = self.run(task).await? {
                self.stats.emitted += 1;
                self.stats.emitted_bytes += object.canonical_bytes.len() as u64;
                return Ok(Some(DeltaStep::Object(object)));
            }
        }
    }

    /// A runnable task of the earliest unfinished stage; `None` when that stage is
    /// waiting for parked tasks (or everything is done).
    fn next_task(&mut self) -> Option<Task> {
        for stage in Stage::ALL {
            if let Some(task) = self.stacks[stage as usize].pop() {
                return Some(task);
            }
            if self.parked.iter().any(|task| task.stage() <= stage) {
                return None;
            }
        }
        None
    }

    fn missing_ids(&self) -> Vec<ObjectId> {
        let mut seen = HashSet::new();
        self.parked
            .iter()
            .map(Task::id)
            .filter(|id| seen.insert(*id))
            .take(self.limits.max_missing_batch)
            .collect()
    }

    fn push(&mut self, task: Task) -> Result<()> {
        let pending: usize = self.stacks.iter().map(Vec::len).sum();
        if pending + self.parked.len() >= self.limits.max_pending {
            return Err(StructuralDeltaError::LimitExceeded("pending tasks"));
        }
        self.stacks[task.stage() as usize].push(task);
        Ok(())
    }

    /// Notes that `id` was skipped as the same as its old counterpart.
    fn prune_aligned(&mut self, id: ObjectId) {
        self.stats.pruned_aligned += 1;
        self.proven_old.insert(id);
    }

    /// Whether `id` was proven old at another place; counts the skip if so.
    fn prune_proven_old(&mut self, id: ObjectId) -> bool {
        let proven = self.proven_old.contains(&id);
        if proven {
            self.stats.pruned_proven_old += 1;
        }
        proven
    }

    /// Pushes a reference unless it is the same as its old counterpart.
    fn push_paired(&mut self, task: Task, old: Option<ObjectId>) -> Result<()> {
        if old == Some(task.id()) {
            self.prune_aligned(task.id());
            return Ok(());
        }
        if self.emitted.contains(&task.id()) {
            self.stats.pruned_emitted += 1;
            return Ok(());
        }
        self.push(task)
    }

    fn push_entity(&mut self, id: ObjectId, entity: Entity, known_new: bool) -> Result<()> {
        if self.emitted.contains(&id) {
            self.stats.pruned_emitted += 1;
            return Ok(());
        }
        self.push(Task::Entity {
            id,
            entity,
            known_new,
        })
    }

    /// Reads a new-side object. `None` means it is missing and the task was parked.
    async fn read_new(
        &mut self,
        task: &Task,
        expected: &'static str,
        kinds: &[ObjectKind],
    ) -> Result<Option<StoredObject>> {
        let id = task.id();
        self.stats.new_reads += 1;
        match self.objects.get(id).await {
            Ok(stored) => {
                if !kinds.contains(&stored.kind) {
                    return Err(StructuralDeltaError::WrongKind {
                        id,
                        expected,
                        actual: stored.kind,
                    });
                }
                Ok(Some(stored))
            }
            Err(ObjectStoreError::Missing(_)) => {
                self.stats.parked += 1;
                self.parked.push(task.clone());
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Reads an old-side object; missing (or the wrong kind) means "no counterpart".
    async fn read_old(
        &mut self,
        id: Option<ObjectId>,
        kind: ObjectKind,
    ) -> Result<Option<StoredObject>> {
        let Some(id) = id else {
            return Ok(None);
        };
        self.stats.old_reads += 1;
        match self.objects.get(id).await {
            Ok(stored) if stored.kind == kind => Ok(Some(stored)),
            Ok(_) => Ok(None),
            Err(ObjectStoreError::Missing(_)) => {
                self.stats.old_missing += 1;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Records `stored` as yielded; `None` if it was yielded already.
    fn emit(&mut self, stored: StoredObject) -> Option<StoredObject> {
        self.emitted.insert(stored.id).then_some(stored)
    }

    async fn run(&mut self, task: Task) -> Result<Option<StoredObject>> {
        match task {
            Task::Envelope { .. } => self.run_envelope(task).await,
            Task::State { .. } => self.run_state(task).await,
            Task::Labels { .. } => self.run_labels(task).await,
            Task::Tree { .. } => self.run_tree(task).await,
            Task::Radix { .. } => self.run_radix(task).await,
            Task::Entity { .. } => self.run_entity(task).await,
        }
    }

    async fn run_envelope(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Task::Envelope { old, .. } = task else {
            unreachable!()
        };
        let Some(stored) = self
            .read_new(
                &task,
                "repository envelope",
                &[ObjectKind::RepositoryEnvelope],
            )
            .await?
        else {
            return Ok(None);
        };
        let record = decode(&stored, wheat_core::decode_repository_envelope_record)?;
        let old = match self.read_old(old, ObjectKind::RepositoryEnvelope).await? {
            Some(old) => Some(decode(&old, wheat_core::decode_repository_envelope_record)?),
            None => None,
        };
        object_graph::envelope(&record, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            match site {
                EnvelopeSite::State => {
                    let old = old.as_ref().map(|old| old.state.into());
                    self.push_paired(Task::State { id, old }, old)
                }
                EnvelopeSite::Labels => self.push_paired(
                    Task::Labels { id },
                    old.as_ref().map(|old| old.labels.into()),
                ),
            }
        })?;
        Ok(self.emit(stored))
    }

    async fn run_state(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Task::State { old, .. } = task else {
            unreachable!()
        };
        let Some(stored) = self
            .read_new(&task, "repository state", &[ObjectKind::RepositoryState])
            .await?
        else {
            return Ok(None);
        };
        let new = decode(&stored, wheat_core::decode_repository_state_record)?;
        let old = match self.read_old(old, ObjectKind::RepositoryState).await? {
            Some(old) => Some(decode(&old, wheat_core::decode_repository_state_record)?),
            None => None,
        };
        if let Some(old) = &old {
            self.old_state = Some(OldState {
                retained: [
                    old.retained_views,
                    old.retained_revisions,
                    old.retained_applications,
                    old.retained_changes,
                ],
                lineage: old.view_lineage,
            });
        }
        let old_field =
            |pick: fn(&wheat_core::RepositoryStateRecord) -> ObjectId| old.as_ref().map(pick);

        object_graph::state(&new, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            let (role, old) = match site {
                StateSite::BaseView => {
                    if old_field(|state| state.base_view.into()) == Some(id) {
                        self.prune_aligned(id);
                        return Ok(());
                    }
                    return self.push_entity(id, Entity::View, false);
                }
                StateSite::RequiredContent => (
                    Role::AggregateContent,
                    old_field(|state| state.required_content.into()),
                ),
                StateSite::RetainedViews => (
                    Role::RetainedViews,
                    old_field(|state| state.retained_views.into()),
                ),
                StateSite::RetainedRevisions => (
                    Role::RetainedRevisions,
                    old_field(|state| state.retained_revisions.into()),
                ),
                StateSite::RetainedApplications => (
                    Role::RetainedApplications,
                    old_field(|state| state.retained_applications.into()),
                ),
                StateSite::RetainedChanges => (
                    Role::RetainedChanges,
                    old_field(|state| state.retained_changes.into()),
                ),
                StateSite::ViewLineage => {
                    (Role::Lineage, old_field(|state| state.view_lineage.into()))
                }
                StateSite::Changes => (Role::ChangeMap, old_field(|state| state.changes.into())),
            };
            self.push_radix_root(id, role, old, None)
        })?;
        Ok(self.emit(stored))
    }

    fn push_radix_root(
        &mut self,
        id: ObjectId,
        role: Role,
        old: Option<ObjectId>,
        parent: Option<ViewId>,
    ) -> Result<()> {
        if old == Some(id) {
            self.prune_aligned(id);
            return Ok(());
        }
        self.push(Task::Radix {
            id,
            role,
            path: KeyPath::ROOT,
            old: old.map_or(OldRadix::Unknown, OldRadix::Id),
            parent,
        })
    }

    async fn run_labels(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Some(stored) = self
            .read_new(
                &task,
                "view label manifest",
                &[ObjectKind::ViewLabelManifest],
            )
            .await?
        else {
            return Ok(None);
        };
        let record = decode(&stored, wheat_core::decode_view_label_manifest_record)?;
        object_graph::labels(&record, |reference| match reference {
            Ref::Object(LabelSite::View, id) => self.push_entity(id, Entity::View, false),
            Ref::Content(_) => Ok(()),
        })?;
        Ok(self.emit(stored))
    }

    async fn run_tree(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Task::Tree { old, .. } = task else {
            unreachable!()
        };
        if self.emitted.contains(&task.id()) {
            self.stats.pruned_emitted += 1;
            return Ok(None);
        }
        if self.prune_proven_old(task.id()) {
            return Ok(None);
        }
        let Some(stored) = self.read_new(&task, "tree", &[ObjectKind::Tree]).await? else {
            return Ok(None);
        };
        let node = decode(&stored, wheat_core::decode_directory_node)?;
        let old = match self.read_old(old, ObjectKind::Tree).await? {
            Some(old) => Some(decode(&old, wheat_core::decode_directory_node)?),
            None => None,
        };
        object_graph::tree(&node, |reference| match reference {
            Ref::Object(TreeSite::Directory(name), id) => {
                let old_child = old.as_ref().and_then(|old| match old.entries.get(name) {
                    Some(DirectoryEntry::Directory(id)) => Some(ObjectId::from(*id)),
                    _ => None,
                });
                self.push_paired(Task::Tree { id, old: old_child }, old_child)
            }
            Ref::Content(_) => Ok(()),
        })?;
        Ok(self.emit(stored))
    }

    async fn run_radix(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Task::Radix {
            id,
            role,
            path,
            ref old,
            parent,
        } = task
        else {
            unreachable!()
        };
        if self.prune_proven_old(id) {
            return Ok(None);
        }
        if !self.radix_walked.insert((id, role, path)) {
            self.stats.pruned_emitted += 1;
            return Ok(None);
        }
        let old = old.clone();
        let Some(stored) = self.read_new(&task, "radix node", &[role.kind()]).await? else {
            self.radix_walked.remove(&(id, role, path));
            return Ok(None);
        };
        let node = decode(&stored, |bytes| role.decode(bytes))?;
        let mut references = std::mem::take(&mut self.radix_references);
        references.clear();
        object_graph::radix(
            role.radix_kind(),
            id,
            &node,
            &path.bytes[..usize::from(path.len)],
            |reference| {
                references.push(reference);
                Ok::<_, StructuralDeltaError>(())
            },
        )?;
        let old_here = self.advance(old, &node.prefix, role).await?;
        let here = path.extended(&node.prefix);

        for reference in references.drain(..) {
            let Ref::Object(site, target) = reference else {
                continue;
            };
            match site {
                RadixSite::Key { entry } => {
                    let (suffix, value, old_value) = match entry {
                        None => (&[][..], node.value.flatten(), old_value_at(&old_here)),
                        Some(index) => {
                            let (suffix, value) = &node.entries[index];
                            let old_there = self.advance(old_here.clone(), suffix, role).await?;
                            (suffix.as_slice(), *value, old_value_at(&old_there))
                        }
                    };
                    self.key_event(
                        role,
                        parent,
                        here.extended(suffix),
                        value,
                        target,
                        old_value,
                    )
                    .await?;
                }
                RadixSite::Child { edge } => {
                    let child_old = match &old_here {
                        OldRadix::At(old, skip) if *skip == old.prefix.len() => {
                            match old.children.iter().find(|(old_edge, _)| *old_edge == edge) {
                                Some(&(_, old_child)) if old_child == target => {
                                    self.prune_aligned(target);
                                    continue;
                                }
                                Some(&(_, old_child)) => OldRadix::Id(old_child),
                                None => keys_below(&old.entries, &[edge]),
                            }
                        }
                        OldRadix::At(old, skip) if old.prefix[*skip] == edge => {
                            OldRadix::At(Arc::clone(old), skip + 1)
                        }
                        OldRadix::At(..) => OldRadix::absent(),
                        OldRadix::Keys(keys) => keys_below(keys.iter(), &[edge]),
                        OldRadix::Unknown | OldRadix::Id(_) => OldRadix::Unknown,
                    };
                    self.push(Task::Radix {
                        id: target,
                        role,
                        path: here.extended(&[edge]),
                        old: child_old,
                        parent,
                    })?;
                }
            }
        }
        self.radix_references = references;
        Ok(self.emit(stored))
    }

    /// The old side `bytes` further down the key: an old node (possibly partway
    /// through its prefix), the old keys there, or unknown. Reads old nodes only
    /// where the old side branches into a child.
    async fn advance(&mut self, old: OldRadix, bytes: &[u8], role: Role) -> Result<OldRadix> {
        let (mut node, mut skip) = match old {
            OldRadix::Unknown => return Ok(OldRadix::Unknown),
            OldRadix::Keys(keys) => return Ok(keys_below(keys.iter(), bytes)),
            OldRadix::Id(id) => match self.read_old_radix(id, role).await? {
                Some(node) => (node, 0),
                None => return Ok(OldRadix::Unknown),
            },
            OldRadix::At(node, skip) => (node, skip),
        };
        let mut consumed = 0;
        loop {
            let rest = &node.prefix[skip..];
            let wanted = &bytes[consumed..];
            let common = rest
                .iter()
                .zip(wanted)
                .take_while(|(left, right)| left == right)
                .count();
            if common == wanted.len() {
                return Ok(OldRadix::At(node, skip + common));
            }
            if common < rest.len() {
                // The key leaves the old prefix: the old side has nothing there.
                return Ok(OldRadix::absent());
            }
            let edge = wanted[common];
            let Some(&(_, child)) = node.children.iter().find(|(old_edge, _)| *old_edge == edge)
            else {
                // Inline entries hold whole key suffixes from the end of the prefix.
                return Ok(keys_below(&node.entries, &wanted[common..]));
            };
            consumed += common + 1;
            match self.read_old_radix(child, role).await? {
                Some(child) => {
                    node = child;
                    skip = 0;
                }
                None => return Ok(OldRadix::Unknown),
            }
        }
    }

    async fn read_old_radix(&mut self, id: ObjectId, role: Role) -> Result<Option<Arc<RadixNode>>> {
        let Some(stored) = self.read_old(Some(id), role.kind()).await? else {
            return Ok(None);
        };
        Ok(Some(Arc::new(decode(&stored, |bytes| role.decode(bytes))?)))
    }

    /// A key of the new collection whose value is new, or not known to be old.
    /// `target` is what the key refers to: the key itself in a set, `value` in a map.
    async fn key_event(
        &mut self,
        role: Role,
        parent: Option<ViewId>,
        key: KeyPath,
        value: Option<ObjectId>,
        target: ObjectId,
        old: OldValue,
    ) -> Result<()> {
        if let OldValue::Present(old_value) = old
            && old_value == value
        {
            self.stats.pruned_aligned += 1;
            return Ok(());
        }
        let known_new = matches!(old, OldValue::Absent);
        match role {
            Role::RetainedViews => self.push_entity(target, Entity::View, known_new),
            Role::RetainedRevisions => self.push_entity(target, Entity::Revision, known_new),
            Role::RetainedApplications => self.push_entity(target, Entity::Application, known_new),
            Role::RetainedChanges => self.push_entity(target, Entity::Change, known_new),
            Role::ConflictSet => self.push_entity(target, Entity::Conflict, false),
            Role::LineageChildren => {
                if let Some(parent) = parent {
                    self.parent_of
                        .entry(ViewId::from_digest(*target.as_bytes()))
                        .or_insert(parent);
                }
                self.push_entity(target, Entity::View, false)
            }
            Role::Lineage => {
                let parent = ViewId::from_digest(key.full());
                let old_children = match old {
                    OldValue::Present(old) => old,
                    OldValue::Absent => None,
                    OldValue::Unknown => self.old_lineage_children(parent).await?,
                };
                self.push_radix_root(target, Role::LineageChildren, old_children, Some(parent))
            }
            Role::ChangeMap => self.push_entity(target, Entity::Change, false),
            Role::RevisionMap => self.push_entity(target, Entity::Revision, false),
            Role::ApplicationMap => self.push_entity(target, Entity::Application, false),
            // Content keys are blobs, which object_graph does not list as references.
            Role::ViewContent | Role::AggregateContent => {
                unreachable!("content collection keys are not references")
            }
        }
    }

    async fn old_lineage_children(&mut self, parent: ViewId) -> Result<Option<ObjectId>> {
        let Some(root) = self.old_state.as_ref().map(|old| old.lineage) else {
            return Ok(None);
        };
        self.stats.old_reads += 1;
        match crate::ViewLineageMapStore::new(self.objects)
            .get(root, parent)
            .await
        {
            Ok(children) => Ok(children.map(Into::into)),
            Err(crate::ViewLineageError::Store(ObjectStoreError::Missing(_))) => {
                self.stats.old_missing += 1;
                Ok(None)
            }
            Err(error) => Err(StructuralDeltaError::OldLineage(error)),
        }
    }

    /// Whether the old state retains `id` in the set for `entity`.
    async fn old_retains(&mut self, entity: Entity, id: ObjectId) -> Result<bool> {
        let (Some(index), Some(old)) = (entity.retained_index(), self.old_state.as_ref()) else {
            return Ok(false);
        };
        let root = old.retained[index];
        if let Some(&known) = self.retained_cache.get(&(index, id)) {
            return Ok(known);
        }
        self.stats.retained_lookups += 1;
        let retained = match RetainedSetStore::new(self.objects).contains(root, id).await {
            Ok(retained) => retained,
            Err(CollectionError::Store(ObjectStoreError::Missing(_))) => {
                self.stats.old_missing += 1;
                false
            }
            Err(error) => return Err(error.into()),
        };
        self.retained_cache.insert((index, id), retained);
        Ok(retained)
    }

    /// The old view whose structures a new view `view` is best compared with: `view`
    /// itself if the old state retains it, else the old view it replaces, else its
    /// nearest old lineage ancestor.
    ///
    /// A new view replaces the old child of its parent's counterpart that ends in the
    /// same change: revising a change, or resolving its conflict, recomposes the views
    /// from that change up, and each new one mostly shares its maps with the old one
    /// in its place rather than with the views below it.
    ///
    /// Answers are remembered for every view on the way up, so a long line of new
    /// views costs one climb, not one per view. A climb longer than
    /// `max_lineage_chain` (or a cycle) gives up and pairs with nothing.
    async fn old_counterpart(&mut self, view: ViewId) -> Result<Option<ViewId>> {
        if self.old_state.is_none() {
            return Ok(None);
        }
        let mut climbed = Vec::new();
        let mut current = view;
        let mut found = loop {
            if let Some(&known) = self.counterparts.get(&current) {
                break known;
            }
            if climbed.len() > self.limits.max_lineage_chain {
                break None;
            }
            if self.old_retains(Entity::View, current.into()).await? {
                break Some(current);
            }
            climbed.push(current);
            match self.parent_of.get(&current) {
                Some(&parent) => current = parent,
                None => break None,
            }
        };
        // Back down the climb, from the view just above `found` to `view`.
        for view in climbed.into_iter().rev() {
            if let Some(parent) = found
                && let Some(replaced) = self.replaced_old_view(parent, view).await?
            {
                found = Some(replaced);
            }
            self.counterparts.insert(view, found);
        }
        Ok(found)
    }

    /// The old child of `parent` whose last change is the new view `view`'s.
    async fn replaced_old_view(&mut self, parent: ViewId, view: ViewId) -> Result<Option<ViewId>> {
        let Some(last) = self.new_last_change(view).await? else {
            return Ok(None);
        };
        if !self.old_children.contains_key(&parent) {
            let children = self.old_children_by_last(parent).await?;
            self.old_children.insert(parent, children);
        }
        Ok(self.old_children[&parent]
            .as_ref()
            .and_then(|children| children.get(&last).copied()))
    }

    /// The last change of the new view `view`, read without parking: a view that is
    /// not here yet just pairs with its ancestor.
    async fn new_last_change(&mut self, view: ViewId) -> Result<Option<ChangeId>> {
        if let Some(&last) = self.new_last.get(&view) {
            return Ok(last);
        }
        self.stats.new_reads += 1;
        let last = match self.objects.get(view.into()).await {
            Ok(stored) if stored.kind == ObjectKind::View => {
                decode(&stored, wheat_core::decode_view_record)?.order.last
            }
            Ok(_) | Err(ObjectStoreError::Missing(_)) => None,
            Err(error) => return Err(error.into()),
        };
        self.new_last.insert(view, last);
        Ok(last)
    }

    /// The old children of `parent` by their last change, or `None` when there are
    /// more than `max_old_children` or they can't all be read.
    async fn old_children_by_last(
        &mut self,
        parent: ViewId,
    ) -> Result<Option<HashMap<ChangeId, ViewId>>> {
        let Some(root) = self.old_lineage_children(parent).await? else {
            return Ok(Some(HashMap::new()));
        };
        let sets = RetainedSetStore::new(self.objects);
        let mut ids = Vec::new();
        let listed = async {
            let mut cursor = sets
                .cursor(RetainedSetId::from_digest(*root.as_bytes()))
                .await?;
            while let Some(child) = cursor.next().await? {
                if ids.len() == self.limits.max_old_children {
                    return Ok(false);
                }
                ids.push(child);
            }
            Ok(true)
        }
        .await;
        match listed {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(CollectionError::Store(ObjectStoreError::Missing(_))) => {
                self.stats.old_missing += 1;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        }
        let mut children = HashMap::new();
        for id in ids {
            let child = ViewId::from_digest(*id.as_bytes());
            let Some(record) = self.old_view(Some(child)).await? else {
                return Ok(None);
            };
            if let Some(last) = record.order.last {
                children.entry(last).or_insert(child);
            }
        }
        Ok(Some(children))
    }

    async fn old_view(&mut self, view: Option<ViewId>) -> Result<Option<wheat_core::ViewRecord>> {
        let Some(view) = view else {
            return Ok(None);
        };
        if let Some(&record) = self.old_views.get(&view) {
            return Ok(record);
        }
        let record = match self.read_old(Some(view.into()), ObjectKind::View).await? {
            Some(stored) => Some(decode(&stored, wheat_core::decode_view_record)?),
            None => None,
        };
        self.old_views.insert(view, record);
        Ok(record)
    }

    /// If the old state retains `revision`, notes its edits as old: they are in the
    /// old closure, wherever else they turn up.
    async fn learn_old_edits(&mut self, revision: RevisionId) -> Result<()> {
        if !self.old_revisions_learned.insert(revision)
            || !self.old_retains(Entity::Revision, revision.into()).await?
        {
            return Ok(());
        }
        if let Some(stored) = self
            .read_old(Some(revision.into()), ObjectKind::Revision)
            .await?
        {
            let record = decode(&stored, wheat_core::decode_revision_record)?;
            let old_edits = &mut self.old_edits;
            object_graph::revision(&record, |reference| {
                if let Ref::Object(RevisionSite::Edit, id) = reference {
                    old_edits.insert(id);
                }
                Ok::<_, StructuralDeltaError>(())
            })?;
        }
        Ok(())
    }

    async fn run_entity(&mut self, task: Task) -> Result<Option<StoredObject>> {
        let Task::Entity {
            id,
            entity,
            known_new,
        } = task
        else {
            unreachable!()
        };
        if self.emitted.contains(&id) {
            self.stats.pruned_emitted += 1;
            return Ok(None);
        }
        if self.prune_proven_old(id) {
            return Ok(None);
        }
        if !known_new && self.old_retains(entity, id).await?
            || entity == Entity::Edit && self.old_edits.contains(&id)
        {
            self.stats.pruned_retained += 1;
            return Ok(None);
        }
        let expected = match entity {
            Entity::View => "view",
            Entity::Revision => "revision",
            Entity::Application => "application",
            Entity::Change => "change",
            Entity::Conflict => "conflict",
            Entity::Edit => "edit",
        };
        let Some(stored) = self.read_new(&task, expected, &[entity.kind()]).await? else {
            return Ok(None);
        };
        match entity {
            Entity::View => self.follow_view(&stored).await?,
            Entity::Revision => self.follow_revision(&stored).await?,
            Entity::Application => self.follow_application(&stored).await?,
            Entity::Conflict => self.follow_conflict(&stored)?,
            // Edits and changes refer to no objects, so they are not decoded. These
            // stop compiling if either gains an object reference.
            Entity::Edit | Entity::Change => {
                const _: fn(EditSite) -> ! = |site| match site {};
                const _: fn(ChangeSite) -> ! = |site| match site {};
            }
        }
        Ok(self.emit(stored))
    }

    async fn follow_view(&mut self, stored: &StoredObject) -> Result<()> {
        let view = decode(stored, wheat_core::decode_view_record)?;
        let id = ViewId::from_digest(*stored.id.as_bytes());
        self.new_last.insert(id, view.order.last);
        let counterpart = self.old_counterpart(id).await?;
        let old = self.old_view(counterpart).await?;
        let old_field = |pick: fn(wheat_core::ViewRecord) -> ObjectId| old.map(pick);
        object_graph::view(&view, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            match site {
                ViewSite::Tree => {
                    let old = old_field(|old| old.tree.into());
                    self.push_paired(Task::Tree { id, old }, old)
                }
                ViewSite::Revisions => self.push_radix_root(
                    id,
                    Role::RevisionMap,
                    old_field(|old| old.revisions.into()),
                    None,
                ),
                ViewSite::Applications => self.push_radix_root(
                    id,
                    Role::ApplicationMap,
                    old_field(|old| old.applications.into()),
                    None,
                ),
                ViewSite::UnresolvedConflicts => self.push_radix_root(
                    id,
                    Role::ConflictSet,
                    old_field(|old| old.unresolved_conflicts.into()),
                    None,
                ),
                ViewSite::RequiredContent => self.push_radix_root(
                    id,
                    Role::ViewContent,
                    old_field(|old| old.required_content.into()),
                    None,
                ),
            }
        })
    }

    /// The tree of the old view a tree based on `view` is best compared with.
    async fn old_tree_for(&mut self, view: ViewId) -> Result<Option<ObjectId>> {
        let counterpart = self.old_counterpart(view).await?;
        Ok(self
            .old_view(counterpart)
            .await?
            .map(|old| ObjectId::from(old.tree)))
    }

    async fn follow_revision(&mut self, stored: &StoredObject) -> Result<()> {
        let revision = decode(stored, wheat_core::decode_revision_record)?;
        // Before the edits are pushed: those the predecessor or the combined inputs had
        // may be old.
        for parent in revision.predecessor.iter().chain(&revision.combines) {
            self.learn_old_edits(*parent).await?;
        }
        let old_tree = self.old_tree_for(revision.authored_against).await?;
        object_graph::revision(&revision, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            match site {
                RevisionSite::Predecessor | RevisionSite::Combines => {
                    self.push_entity(id, Entity::Revision, false)
                }
                RevisionSite::AuthoredAgainst => self.push_entity(id, Entity::View, false),
                RevisionSite::Tree => self.push_paired(Task::Tree { id, old: old_tree }, old_tree),
                RevisionSite::Edit => self.push_edit(id),
                RevisionSite::ResolutionApplication => {
                    self.push_entity(id, Entity::Application, false)
                }
            }
        })
    }

    async fn follow_application(&mut self, stored: &StoredObject) -> Result<()> {
        let application = decode(stored, wheat_core::decode_application_record)?;
        // Before the conflict below is followed: its edits are mostly the revision's.
        self.learn_old_edits(application.revision_id).await?;
        let old_tree = self.old_tree_for(application.target_view).await?;
        object_graph::application(&application, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            match site {
                ApplicationSite::Revision => self.push_entity(id, Entity::Revision, false),
                ApplicationSite::TargetView => self.push_entity(id, Entity::View, false),
                ApplicationSite::Skip => self.push_entity(id, Entity::Application, false),
                ApplicationSite::Tree => {
                    self.push_paired(Task::Tree { id, old: old_tree }, old_tree)
                }
                ApplicationSite::Conflict => self.push_entity(id, Entity::Conflict, false),
                ApplicationSite::Edit => self.push_edit(id),
            }
        })
    }

    fn follow_conflict(&mut self, stored: &StoredObject) -> Result<()> {
        let conflict = decode(stored, wheat_core::decode_conflict)?;
        object_graph::conflict(&conflict, |reference| {
            let Ref::Object(site, id) = reference else {
                return Ok(());
            };
            match site {
                ConflictSite::BaseView => self.push_entity(id, Entity::View, false),
                ConflictSite::Edit => self.push_edit(id),
            }
        })
    }

    fn push_edit(&mut self, edit: ObjectId) -> Result<()> {
        if self.old_edits.contains(&edit) {
            self.stats.pruned_retained += 1;
            return Ok(());
        }
        self.push_entity(edit, Entity::Edit, false)
    }
}

/// The set a content-set delta applies to; `None` for a snapshot.
fn decode<T>(
    stored: &StoredObject,
    decoder: impl FnOnce(&[u8]) -> std::result::Result<T, CanonicalObjectError>,
) -> Result<T> {
    decoder(&stored.canonical_bytes).map_err(|source| StructuralDeltaError::Decode {
        id: stored.id,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Durability, LooseObjectStore};

    fn key(bytes: &[u8]) -> ObjectId {
        let mut digest = [0; 32];
        digest[..bytes.len()].copy_from_slice(bytes);
        ObjectId::from_digest(digest)
    }

    async fn set(objects: &LooseObjectStore, keys: &[ObjectId]) -> RetainedSetId {
        let sets = RetainedSetStore::new(objects);
        let empty = sets.empty_root().await.unwrap();
        sets.update_batch(empty, keys.iter().map(|key| (*key, true)))
            .await
            .unwrap()
    }

    async fn node(objects: &LooseObjectStore, id: impl Into<ObjectId>) -> RadixNode {
        let stored = objects.get(id.into()).await.unwrap();
        Role::RetainedViews.decode(&stored.canonical_bytes).unwrap()
    }

    /// Walks one retained-view set against another. Returns the nodes yielded and the
    /// keys it could not prove old (their views are not in the store, so they park).
    async fn walk_set(
        objects: &LooseObjectStore,
        old: Option<RetainedSetId>,
        new: RetainedSetId,
    ) -> (HashSet<ObjectId>, HashSet<ObjectId>) {
        walk_sets(objects, &[(Role::RetainedViews, old, new)]).await
    }

    /// [`walk_set`] over several sets, walked in order.
    async fn walk_sets(
        objects: &LooseObjectStore,
        sets: &[(Role, Option<RetainedSetId>, RetainedSetId)],
    ) -> (HashSet<ObjectId>, HashSet<ObjectId>) {
        let nothing = RepositoryEnvelopeId::from_digest([0; 32]);
        let mut walk = StructuralDelta::new(objects, Some(nothing), nothing, Default::default());
        // Tasks run last pushed first.
        for &(role, old, new) in sets.iter().rev() {
            walk.push_radix_root(new.into(), role, old.map(Into::into), None)
                .unwrap();
        }
        let mut yielded = HashSet::new();
        while let Some(step) = walk.next().await.unwrap() {
            match step {
                DeltaStep::Object(object) => {
                    assert_eq!(object.kind, ObjectKind::RetainedSetNode);
                    yielded.insert(object.id);
                }
                DeltaStep::Missing(keys) => return (yielded, keys.into_iter().collect()),
            }
        }
        (yielded, HashSet::new())
    }

    fn store() -> (tempfile::TempDir, LooseObjectStore) {
        let dir = tempfile::tempdir().unwrap();
        let objects = LooseObjectStore::with_durability(dir.path(), Durability::Deferred);
        (dir, objects)
    }

    #[tokio::test]
    async fn an_old_node_whose_prefix_the_new_one_splits_still_pairs() {
        let (_dir, objects) = store();
        let near = [key(&[7, 7, 7, 7, 7, 1]), key(&[7, 7, 7, 7, 7, 2])];
        let far = key(&[7, 7, 9]);
        let old = set(&objects, &near).await;
        let new = set(&objects, &[near[0], near[1], far]).await;
        // The new root branches after two bytes, inside the old root's longer prefix.
        assert!(node(&objects, new).await.prefix.len() < node(&objects, old).await.prefix.len());

        let (yielded, unproven) = walk_set(&objects, Some(old), new).await;
        assert_eq!(unproven, [far].into());
        assert!(yielded.contains(&new.into()));

        // And the other way round: the new root's prefix runs through an old child.
        let (_, unproven) = walk_set(&objects, Some(new), old).await;
        assert!(unproven.is_empty(), "{unproven:?}");
    }

    #[tokio::test]
    async fn one_subtree_under_two_edges_is_walked_under_both() {
        let (_dir, objects) = store();
        let keys = [
            key(&[1, 5, 5, 5, 1]),
            key(&[1, 5, 5, 5, 2]),
            key(&[2, 5, 5, 5, 1]),
            key(&[2, 5, 5, 5, 2]),
        ];
        let root = set(&objects, &keys).await;
        let children = node(&objects, root).await.children;
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[0].1, children[1].1,
            "the two subtrees share one node"
        );

        let (yielded, unproven) = walk_set(&objects, None, root).await;
        assert_eq!(unproven, keys.into());
        assert_eq!(yielded, [root.into(), children[0].1].into());
    }

    /// One node can sit in two collections. Once the walk of one pairs it with the
    /// same ID, the walk of the other skips it too, though its old side there is a
    /// node of another shape.
    #[tokio::test]
    async fn a_node_proven_old_in_one_collection_is_skipped_in_another() {
        let (_dir, objects) = store();
        let near = [key(&[2, 5, 5, 5, 1]), key(&[2, 5, 5, 5, 2])];
        let views_old = set(&objects, &[near[0], near[1], key(&[1])]).await;
        let views_new = set(&objects, &[near[0], near[1], key(&[1]), key(&[3])]).await;
        let children_old = set(&objects, &near).await;
        let children_new = set(&objects, &[near[0], near[1], key(&[4])]).await;

        let edge_two = |node: RadixNode| {
            node.children
                .iter()
                .find(|(edge, _)| *edge == 2)
                .map(|&(_, child)| child)
        };
        let shared = edge_two(node(&objects, views_new).await).unwrap();
        assert_eq!(edge_two(node(&objects, views_old).await), Some(shared));
        assert_eq!(edge_two(node(&objects, children_new).await), Some(shared));
        // The old set of children holds `near` in its root, so `shared` has no pair there.
        assert!(node(&objects, children_old).await.children.is_empty());

        let (yielded, unproven) = walk_sets(
            &objects,
            &[
                (Role::RetainedViews, Some(views_old), views_new),
                (Role::LineageChildren, Some(children_old), children_new),
            ],
        )
        .await;
        assert_eq!(yielded, [views_new.into(), children_new.into()].into());
        assert_eq!(unproven, [key(&[3]), key(&[4])].into());
    }

    #[tokio::test]
    async fn only_changed_keys_are_left_unproven() {
        let (_dir, objects) = store();
        let keys: Vec<ObjectId> = (0..600u32)
            .map(|index| ObjectId::from_digest(*blake3::hash(&index.to_le_bytes()).as_bytes()))
            .collect();
        let old = set(&objects, &keys[..500]).await;
        let new = set(&objects, &keys[100..]).await;
        let (_, unproven) = walk_set(&objects, Some(old), new).await;
        assert_eq!(unproven, keys[500..].iter().copied().collect());
        // Backwards, removed keys collapse children into inline entries.
        let (_, unproven) = walk_set(&objects, Some(new), old).await;
        assert_eq!(unproven, keys[..100].iter().copied().collect());
    }
}
