use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, ChangeId, ConflictId, ContentId, ContentManifestId, RevisionId, ViewId,
};

pub type Tree = BTreeMap<String, ContentId>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DirectoryEntry {
    File(ContentId),
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: String,
    pub expected: Option<ContentId>,
    pub replacement: Option<ContentId>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Transformation {
    pub edits: Vec<FileEdit>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ChangeRevision {
    pub(crate) id: RevisionId,
    pub(crate) change_id: ChangeId,
    pub(crate) metadata: RevisionMetadata,
    pub(crate) predecessor: Option<RevisionId>,
    pub(crate) dependencies: BTreeSet<ChangeId>,
    pub(crate) authored_against: ViewId,
    pub(crate) transformation: Transformation,
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
    pub fn transformation(&self) -> &Transformation {
        &self.transformation
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
    pub depth: u8,
    pub value: Option<RevisionId>,
    pub children: Vec<RevisionMapChild>,
}

impl RevisionMapNode {
    pub fn new(
        depth: u8,
        value: Option<RevisionId>,
        mut children: Vec<RevisionMapChild>,
    ) -> Result<Self, crate::CanonicalObjectError> {
        children.sort_by_key(|child| child.edge);
        let node = Self {
            depth,
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PathConflict {
    pub path: String,
    pub expected: Option<ContentId>,
    pub actual: Option<ContentId>,
    pub proposed: Option<ContentId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: ConflictId,
    pub base_view: ViewId,
    pub paths: Vec<PathConflict>,
}

impl Conflict {
    #[must_use]
    pub fn new(base_view: ViewId, mut paths: Vec<PathConflict>) -> Self {
        paths.sort_by(|left, right| left.path.cmp(&right.path));
        assert!(
            paths.windows(2).all(|paths| paths[0].path != paths[1].path),
            "a conflict may contain only one entry per path"
        );
        let mut conflict = Self {
            id: ConflictId::from_digest([0; 32]),
            base_view,
            paths,
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
}

impl Application {
    #[must_use]
    pub fn new(revision_id: RevisionId, target_view: ViewId, result: ApplicationResult) -> Self {
        let mut application = Self {
            id: ApplicationId::from_digest([0; 32]),
            revision_id,
            target_view,
            result,
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
        let required_content = tree.values().copied().collect::<BTreeSet<_>>();
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
