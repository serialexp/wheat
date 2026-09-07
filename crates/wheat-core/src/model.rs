use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, ChangeId, ConflictId, ContentId, ContentManifestId, EditId, EditRecord,
    RevisionId, ViewId,
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
}

impl RevisionMapNode {
    pub fn new(
        prefix: Vec<u8>,
        value: Option<RevisionId>,
        mut children: Vec<RevisionMapChild>,
    ) -> Result<Self, crate::CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        let node = Self {
            prefix,
            value,
            children,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedEditEvidence {
    pub target: Option<FileValue>,
    pub reason: EditFailure,
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
    pub result: ApplicationResult,
    pub applied: BTreeMap<EditId, AppliedEditEvidence>,
}

impl Application {
    #[must_use]
    pub fn new(
        revision_id: RevisionId,
        target_view: ViewId,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub id: ViewId,
    pub tree: Tree,
    pub frontier: BTreeMap<ChangeId, RevisionId>,
    pub applications: BTreeMap<ChangeId, ApplicationId>,
    pub unresolved_conflicts: BTreeSet<ConflictId>,
    pub required_content: BTreeSet<ContentId>,
}

impl RepositoryView {
    #[must_use]
    pub fn new(tree: Tree, frontier: BTreeMap<ChangeId, RevisionId>) -> Self {
        Self::from_manifest(tree, frontier, BTreeMap::new(), BTreeSet::new())
    }

    #[must_use]
    pub fn from_manifest(
        tree: Tree,
        frontier: BTreeMap<ChangeId, RevisionId>,
        applications: BTreeMap<ChangeId, ApplicationId>,
        unresolved_conflicts: BTreeSet<ConflictId>,
    ) -> Self {
        let required_content = tree
            .values()
            .map(|value| value.content)
            .collect::<BTreeSet<_>>();
        let mut view = Self {
            id: ViewId::from_digest([0; 32]),
            tree,
            frontier,
            applications,
            unresolved_conflicts,
            required_content,
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
    pub working_view: ViewId,
    pub published_view: ViewId,
    pub required_content: ContentManifestId,
    pub retained_changes: BTreeSet<crate::ChangeObjectId>,
    pub retained_views: BTreeSet<ViewId>,
    pub retained_revisions: BTreeSet<RevisionId>,
    pub retained_applications: BTreeSet<ApplicationId>,
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
