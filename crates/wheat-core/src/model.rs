use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, ChangeId, ConflictId, ContentId, ContentSetId, EditId, EditRecord, RevisionId,
    ViewId,
};

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum FileKind {
    #[default]
    Regular,
    Executable,
    Symlink,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct FileValue {
    pub content: ContentId,
    pub kind: FileKind,
}

pub type Tree = BTreeMap<String, FileValue>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryEntry {
    File(FileValue),
    Directory(crate::TreeId),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DirectoryNode {
    pub entries: BTreeMap<String, DirectoryEntry>,
}

impl DirectoryNode {
    pub fn new(
        entries: impl IntoIterator<Item = (String, DirectoryEntry)>,
    ) -> Result<Self, crate::CanonicalObjectError> {
        let mut node = Self::default();
        for (name, entry) in entries {
            crate::validate_directory_component(&name)?;
            if node.entries.insert(name, entry).is_some() {
                return Err(crate::CanonicalObjectError::InvalidCollection(
                    "duplicate directory entry",
                ));
            }
        }
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::directory_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> crate::TreeId {
        crate::TreeId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Timestamp {
    pub seconds: i64,
    pub offset_minutes: i16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SourceProvenance {
    pub system: String,
    pub object_id: Vec<u8>,
    pub parent_object_ids: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeMetadata {
    pub title: String,
    pub description: String,
    pub creator: Actor,
    pub created_at: Timestamp,
    pub provenance: Option<SourceProvenance>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionMetadata {
    pub message: String,
    pub author: Actor,
    pub authored_at: Timestamp,
    pub committer: Actor,
    pub committed_at: Timestamp,
    pub provenance: Option<SourceProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub id: ChangeId,
    pub metadata: ChangeMetadata,
}

impl Change {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::change_bytes(self)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditTransition {
    pub preserved: BTreeSet<EditId>,
    pub added: BTreeSet<EditId>,
    pub removed: BTreeSet<EditId>,
    pub subsumed: Vec<EditSubsumption>,
    pub resolutions: Vec<ConflictResolutionEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditSubsumption {
    pub source: EditId,
    pub replacements: BTreeSet<EditId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConflictResolutionEvidence {
    pub application: ApplicationId,
    pub conflicted_edits: BTreeSet<EditId>,
    pub replacement_edits: BTreeSet<EditId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionEdits {
    pub edits: BTreeMap<EditId, EditRecord>,
    pub transition: EditTransition,
}

impl RevisionEdits {
    pub fn validate(&self) -> Result<(), crate::CanonicalObjectError> {
        let selected = self.edits.keys().copied().collect::<BTreeSet<_>>();
        self.transition.validate(&selected)?;
        if self.edits.iter().any(|(id, edit)| *id != edit.id()) {
            return Err(crate::CanonicalObjectError::InvalidCollection(
                "revision edit key does not match edit identity",
            ));
        }
        Ok(())
    }
}

impl EditTransition {
    pub fn validate(&self, selected: &BTreeSet<EditId>) -> Result<(), crate::CanonicalObjectError> {
        if !self.preserved.is_disjoint(&self.added)
            || !self.preserved.is_disjoint(&self.removed)
            || !self.added.is_disjoint(&self.removed)
            || selected != &self.preserved.union(&self.added).copied().collect()
        {
            return Err(crate::CanonicalObjectError::InvalidCollection(
                "invalid edit transition partition",
            ));
        }
        for item in &self.subsumed {
            if item.replacements.is_empty()
                || !self.removed.contains(&item.source)
                || !item.replacements.is_subset(&self.added)
            {
                return Err(crate::CanonicalObjectError::InvalidCollection(
                    "invalid edit subsumption",
                ));
            }
        }
        for item in &self.resolutions {
            if item.conflicted_edits.is_empty() || !item.replacement_edits.is_subset(selected) {
                return Err(crate::CanonicalObjectError::InvalidCollection(
                    "invalid conflict resolution evidence",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeRevision {
    pub(crate) id: RevisionId,
    pub(crate) change_id: ChangeId,
    pub(crate) metadata: RevisionMetadata,
    pub(crate) predecessor: Option<RevisionId>,
    pub(crate) combines: BTreeSet<RevisionId>,
    pub(crate) generation: u64,
    pub(crate) dependencies: BTreeSet<ChangeId>,
    pub(crate) authored_against: ViewId,
    pub(crate) edits: RevisionEdits,
    pub(crate) resulting_tree: Tree,
}

impl ChangeRevision {
    #[must_use]
    pub fn id(&self) -> RevisionId {
        self.id
    }

    #[must_use]
    pub fn change_id(&self) -> ChangeId {
        self.change_id
    }

    #[must_use]
    pub fn metadata(&self) -> &RevisionMetadata {
        &self.metadata
    }

    #[must_use]
    pub fn predecessor(&self) -> Option<RevisionId> {
        self.predecessor
    }

    /// A combined revision's inputs other than its predecessor.
    #[must_use]
    pub fn combines(&self) -> &BTreeSet<RevisionId> {
        &self.combines
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn dependencies(&self) -> &BTreeSet<ChangeId> {
        &self.dependencies
    }

    #[must_use]
    pub fn authored_against(&self) -> ViewId {
        self.authored_against
    }

    #[must_use]
    pub fn edits(&self) -> &BTreeMap<EditId, EditRecord> {
        &self.edits.edits
    }

    #[must_use]
    pub fn edit_transition(&self) -> &EditTransition {
        &self.edits.transition
    }

    #[must_use]
    pub fn resulting_tree(&self) -> &Tree {
        &self.resulting_tree
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::revision_bytes(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionMapChild {
    pub edge: u8,
    pub child: crate::RevisionMapId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RevisionMapNode {
    pub prefix: Vec<u8>,
    pub value: Option<RevisionId>,
    pub children: Vec<RevisionMapChild>,
    pub entries: Vec<(Vec<u8>, RevisionId)>,
}

impl RevisionMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<RevisionId>,
        mut children: Vec<RevisionMapChild>,
        mut entries: Vec<(Vec<u8>, RevisionId)>,
    ) -> Result<Self, crate::CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let node = Self {
            prefix,
            value,
            children,
            entries,
        };
        crate::wire::validate_revision_map_node(&node)?;
        Ok(node)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::revision_map_node_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> crate::RevisionMapId {
        crate::RevisionMapId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppliedEditEvidence {
    pub target: Option<FileValue>,
    pub read: Option<ByteRange>,
    pub write: Option<ByteRange>,
}

impl AppliedEditEvidence {
    pub fn validate(&self) -> Result<(), crate::CanonicalObjectError> {
        if self.read.is_some_and(|r| r.start > r.end) || self.write.is_some_and(|r| r.start > r.end)
        {
            return Err(crate::CanonicalObjectError::InvalidCollection(
                "invalid byte range",
            ));
        }
        if let Some(write) = self.write {
            let Some(read) = self.read else {
                return Err(crate::CanonicalObjectError::InvalidCollection(
                    "write range requires read range",
                ));
            };
            if write.start < read.start || write.end > read.end {
                return Err(crate::CanonicalObjectError::InvalidCollection(
                    "write range must be contained in read range",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EditFailure {
    NoMatch,
    Ambiguous,
    Interference { with: BTreeSet<EditId> },
    KindMismatch,
    ExpectedFileMismatch,
}

impl fmt::Display for EditFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatch => formatter.write_str("the guarded text is not found"),
            Self::Ambiguous => formatter.write_str("the guarded text matches more than once"),
            Self::Interference { with } => {
                write!(formatter, "the edit overlaps {} other edit(s)", with.len())
            }
            Self::KindMismatch => formatter.write_str("the file kind differs from the edit's"),
            Self::ExpectedFileMismatch => {
                formatter.write_str("the current file differs from the edit's expected file")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedEditEvidence {
    pub target: Option<FileValue>,
    pub reason: EditFailure,
}

/// An authored edit that does not apply to the view it is authored against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectedEdit {
    pub path: String,
    /// The file the edit found at `path`, if any.
    pub target: Option<FileValue>,
    pub reason: EditFailure,
}

impl RejectedEdit {
    /// The edits of `failed` with their paths from `edits`, ordered by path.
    ///
    /// # Panics
    ///
    /// When `failed` names an edit missing from `edits`; the failures come from
    /// applying those edits.
    #[must_use]
    pub fn collect(
        edits: &BTreeMap<EditId, EditRecord>,
        failed: &BTreeMap<EditId, FailedEditEvidence>,
    ) -> Vec<Self> {
        let mut rejected = failed
            .iter()
            .map(|(id, evidence)| Self {
                path: edits[id].path.clone(),
                target: evidence.target,
                reason: evidence.reason.clone(),
            })
            .collect::<Vec<_>>();
        rejected.sort_by(|left, right| left.path.cmp(&right.path));
        rejected
    }
}

impl fmt::Display for RejectedEdit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = &self.path;
        match (&self.reason, self.target) {
            (EditFailure::KindMismatch | EditFailure::ExpectedFileMismatch, None) => write!(
                formatter,
                "{path}: the edit expects a file, but the path does not exist"
            ),
            (EditFailure::ExpectedFileMismatch, Some(_)) => write!(
                formatter,
                "{path}: the current file differs from the edit's expected file (the path exists)"
            ),
            (reason, _) => write!(formatter, "{path}: {reason}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: ConflictId,
    pub base_view: ViewId,
    pub edits: BTreeMap<EditId, FailedEditEvidence>,
}

impl Conflict {
    #[must_use]
    pub fn new(base_view: ViewId, edits: BTreeMap<EditId, FailedEditEvidence>) -> Self {
        let mut conflict = Self {
            id: ConflictId::from_digest([0; 32]),
            base_view,
            edits,
        };
        conflict.id = ConflictId::of_canonical_bytes(&crate::wire::conflict_bytes(&conflict));
        conflict
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::conflict_bytes(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ApplicationResult {
    Clean {
        tree: Tree,
    },
    Conflicted {
        partial_tree: Tree,
        conflict: Conflict,
    },
}

impl ApplicationResult {
    #[must_use]
    pub fn materialized_tree(&self) -> &Tree {
        match self {
            Self::Clean { tree } => tree,
            Self::Conflicted { partial_tree, .. } => partial_tree,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Application {
    pub id: ApplicationId,
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub order: ApplicationOrder,
    pub result: ApplicationResult,
    pub applied: BTreeMap<EditId, AppliedEditEvidence>,
}

impl Application {
    #[must_use]
    pub fn new(
        revision_id: RevisionId,
        target_view: ViewId,
        order: ApplicationOrder,
        result: ApplicationResult,
        applied: BTreeMap<EditId, AppliedEditEvidence>,
    ) -> Self {
        assert!(
            applied.values().all(|evidence| evidence.validate().is_ok()),
            "application contains invalid edit evidence"
        );
        let mut application = Self {
            id: ApplicationId::from_digest([0; 32]),
            revision_id,
            target_view,
            order,
            result,
            applied,
        };
        application.id =
            ApplicationId::of_canonical_bytes(&crate::wire::application_bytes(&application));
        application
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::application_bytes(self)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct DependencyGraph {
    parents: BTreeMap<ChangeId, BTreeSet<ChangeId>>,
}

impl DependencyGraph {
    pub(crate) fn add_change(&mut self, change: ChangeId, parents: BTreeSet<ChangeId>) {
        self.parents.insert(change, parents);
    }

    #[must_use]
    pub fn parents(&self, change: ChangeId) -> Option<&BTreeSet<ChangeId>> {
        self.parents.get(&change)
    }

    pub(crate) fn children(&self, change: ChangeId) -> impl Iterator<Item = ChangeId> + '_ {
        self.parents
            .iter()
            .filter_map(move |(candidate, parents)| parents.contains(&change).then_some(*candidate))
    }
}

/// The most inputs a combined revision has besides its predecessor.
pub const MAX_COMBINES: usize = 64;

/// A revision's generation: 1 + the largest generation among its parents (its
/// predecessor and the revisions it combines), 0 with none. `None` on overflow.
#[must_use]
pub fn next_generation(parents: impl IntoIterator<Item = u64>) -> Option<u64> {
    parents
        .into_iter()
        .max()
        .map_or(Some(0), |highest| highest.checked_add(1))
}

/// A view's place in its history, determined by its revision map: how many changes
/// it has, the last of them in the canonical order, and its weight, the sum over its
/// changes of 1 + their revision's generation. Every lineage edge increases the
/// weight, so the lineage cannot hold a cycle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewOrder {
    pub size: u64,
    pub last: Option<ChangeId>,
    pub weight: u64,
}

impl ViewOrder {
    /// The base view's: no changes.
    pub const BASE: Self = Self {
        size: 0,
        last: None,
        weight: 0,
    };

    /// The order of this view with `change` applied last, at a revision of
    /// `generation`. `None` on overflow.
    #[must_use]
    pub fn then(self, change: ChangeId, generation: u64) -> Option<Self> {
        Some(Self {
            size: self.size.checked_add(1)?,
            last: Some(change),
            weight: self.weight.checked_add(generation.checked_add(1)?)?,
        })
    }
}

/// An application's place in the canonical order of the view it produces, and an index
/// over the changes before it there: for each `j` that fits, the application `2^j`
/// positions back and the highest ChangeId among the `2^j` positions ending at this
/// one. Determined by the target view, so the same in every history reaching it. It
/// finds the highest ChangeId over any run of a view's last positions in O(log n)
/// application reads ([`RangeMax`]), which is how a view is checked to hold its last
/// change last in canonical order.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApplicationOrder {
    /// The change's index in its result view's canonical order: the target's size.
    pub position: u64,
    /// The target's producing application, `skip[0]`; none on the base.
    pub previous: Option<ApplicationId>,
    /// `skip[j]` is the application at `position - 2^j`, for each `2^j <= position`.
    pub skip: Vec<ApplicationId>,
    /// `window_max[j]` is the highest ChangeId at positions `position - 2^j + 1` through
    /// `position` (from 0 where that starts below), for each `2^j <= position + 1`.
    pub window_max: Vec<ChangeId>,
}

/// An application order index that does not line up with the applications it names.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("application order index does not line up with the applications it names")]
pub struct OrderIndexMismatch;

impl ApplicationOrder {
    /// How many `skip` entries an application at `position` has.
    #[must_use]
    pub fn skip_levels(position: u64) -> usize {
        position.checked_ilog2().map_or(0, |log| log as usize + 1)
    }

    /// How many `window_max` entries an application at `position` has.
    #[must_use]
    pub fn window_levels(position: u64) -> usize {
        (u128::from(position) + 1).ilog2() as usize + 1
    }

    /// Whether the entries' counts fit the position and `previous` is `skip[0]`.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        self.skip.len() == Self::skip_levels(self.position)
            && self.window_max.len() == Self::window_levels(self.position)
            && self.previous == self.skip.first().copied()
    }

    /// Starts the order of `change` applied onto a view of `position` changes whose
    /// producing application is `previous`. Complete it by passing the order of each
    /// application [`Self::wanted`] names to [`Self::extend`].
    ///
    /// # Panics
    ///
    /// When `previous` is missing for a non-base target or given for the base.
    #[must_use]
    pub fn begin(change: ChangeId, position: u64, previous: Option<ApplicationId>) -> Self {
        assert_eq!(
            previous.is_some(),
            position > 0,
            "exactly the base view has no producing application"
        );
        Self {
            position,
            previous,
            skip: previous.into_iter().collect(),
            window_max: vec![change],
        }
    }

    /// The application whose order the next level needs, until the index is complete.
    #[must_use]
    pub fn wanted(&self) -> Option<ApplicationId> {
        (self.window_max.len() < Self::window_levels(self.position))
            .then(|| self.skip[self.window_max.len() - 1])
    }

    /// Adds the next level from `node`, the order of the application [`Self::wanted`]
    /// named.
    ///
    /// # Errors
    ///
    /// When `node` is not at the position that application must be at, or is not well
    /// formed.
    pub fn extend(&mut self, node: &Self) -> Result<(), OrderIndexMismatch> {
        let level = self.window_max.len();
        let distance = 1_u64 << (level - 1);
        if node.position != self.position - distance || !node.is_well_formed() {
            return Err(OrderIndexMismatch);
        }
        self.window_max
            .push(self.window_max[level - 1].max(node.window_max[level - 1]));
        if self.skip.len() < Self::skip_levels(self.position) {
            self.skip.push(node.skip[level - 1]);
        }
        Ok(())
    }
}

/// The highest ChangeId at a view's positions `from` onwards, found from the order
/// index by walking back from the view's producing application: start with that
/// application's order and pass each order [`Self::fold`] asks for next.
#[derive(Clone, Copy, Debug)]
pub struct RangeMax {
    from: u64,
    /// Where the next folded application must be.
    expected: u64,
    highest: Option<ChangeId>,
}

impl RangeMax {
    /// Starts a query over positions `from` up to a view's `size`.
    #[must_use]
    pub fn new(from: u64, size: u64) -> Self {
        Self {
            from,
            expected: size.wrapping_sub(1),
            highest: None,
        }
    }

    /// Whether the range holds no position, so nothing need be read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.from > self.expected || self.expected == u64::MAX
    }

    /// Folds in the longest window ending at `node` that stays in the range, and
    /// returns the application to fold next, if the range has positions left.
    ///
    /// # Errors
    ///
    /// When `node` is not at the expected position or is not well formed.
    pub fn fold(
        &mut self,
        node: &ApplicationOrder,
    ) -> Result<Option<ApplicationId>, OrderIndexMismatch> {
        if self.is_empty() || node.position != self.expected || !node.is_well_formed() {
            return Err(OrderIndexMismatch);
        }
        let remaining = node.position - self.from + 1;
        // `remaining <= position + 1`, so the window exists.
        let level = remaining.ilog2() as usize;
        let window = node.window_max[level];
        self.highest = Some(self.highest.map_or(window, |highest| highest.max(window)));
        let length = 1_u64 << level;
        if remaining == length {
            self.expected = u64::MAX;
            return Ok(None);
        }
        // Positions remain below the window, so `2^level <= position` and the skip exists.
        self.expected = node.position - length;
        Ok(Some(node.skip[level]))
    }

    /// The highest ChangeId folded so far; the answer once [`Self::fold`] returns `None`.
    #[must_use]
    pub fn highest(&self) -> Option<ChangeId> {
        self.highest
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub id: ViewId,
    pub tree: Tree,
    pub frontier: BTreeMap<ChangeId, RevisionId>,
    pub applications: BTreeMap<ChangeId, ApplicationId>,
    pub unresolved_conflicts: BTreeSet<ConflictId>,
    /// Each blob the tree names, with the number of tree entries naming it.
    pub required_content: BTreeMap<ContentId, u32>,
    pub order: ViewOrder,
}

impl RepositoryView {
    #[must_use]
    pub fn new(tree: Tree, frontier: BTreeMap<ChangeId, RevisionId>, order: ViewOrder) -> Self {
        Self::from_manifest(tree, frontier, BTreeMap::new(), BTreeSet::new(), order)
    }

    #[must_use]
    pub fn from_manifest(
        tree: Tree,
        frontier: BTreeMap<ChangeId, RevisionId>,
        applications: BTreeMap<ChangeId, ApplicationId>,
        unresolved_conflicts: BTreeSet<ConflictId>,
        order: ViewOrder,
    ) -> Self {
        let mut required_content = BTreeMap::new();
        for value in tree.values() {
            *required_content.entry(value.content).or_insert(0) += 1;
        }
        let mut view = Self {
            id: ViewId::from_digest([0; 32]),
            tree,
            frontier,
            applications,
            unresolved_conflicts,
            required_content,
            order,
        };
        view.id = ViewId::of_canonical_bytes(&crate::wire::view_bytes(&view));
        view
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::view_bytes(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryState {
    pub id: crate::RepositoryStateId,
    pub base_view: ViewId,
    pub change_objects: BTreeMap<ChangeId, crate::ChangeObjectId>,
    pub view_lineage: crate::ViewLineageMapId,
    pub required_content: ContentSetId,
    pub retained_changes: BTreeSet<crate::ChangeObjectId>,
    pub retained_views: BTreeSet<ViewId>,
    pub retained_revisions: BTreeSet<RevisionId>,
    pub retained_applications: BTreeSet<ApplicationId>,
    pub projected_commits: BTreeSet<crate::ProjectedCommit>,
}

impl RepositoryState {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::repository_state_bytes(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalObject {
    pub id: crate::ObjectId,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryClosure {
    pub state: RepositoryState,
    pub objects: Vec<CanonicalObject>,
    pub required_content: BTreeSet<ContentId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictResolution {
    pub conflict_revision: RevisionId,
    pub resolved_tree: Tree,
}
