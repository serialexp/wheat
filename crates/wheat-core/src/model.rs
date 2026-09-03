use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ApplicationId, ChangeId, ConflictId, ContentId, RevisionId, ViewId};

pub type Tree = BTreeMap<String, ContentId>;

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
    pub id: RevisionId,
    pub change_id: ChangeId,
    pub predecessor: Option<RevisionId>,
    pub authored_against: ViewId,
    pub transformation: Transformation,
    pub resulting_tree: Tree,
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
    pub fn new(base_view: ViewId, paths: Vec<PathConflict>) -> Self {
        let mut canonical = b"wheat-conflict-v1\0".to_vec();
        canonical.extend_from_slice(base_view.to_string().as_bytes());
        for path in &paths {
            canonical.push(0xff);
            canonical.extend_from_slice(path.path.as_bytes());
            for term in [path.expected, path.actual, path.proposed] {
                canonical.push(0);
                if let Some(content) = term {
                    canonical.extend_from_slice(content.to_string().as_bytes());
                }
            }
        }
        Self {
            id: ConflictId::of_canonical_bytes(&canonical),
            base_view,
            paths,
        }
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
        let mut canonical = b"wheat-application-v1\0".to_vec();
        canonical.extend_from_slice(revision_id.to_string().as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(target_view.to_string().as_bytes());
        canonical.push(0);
        match &result {
            ApplicationResult::Clean { tree } => {
                canonical.push(0);
                append_tree(&mut canonical, tree);
            }
            ApplicationResult::Conflicted {
                partial_tree,
                conflict,
            } => {
                canonical.push(1);
                append_tree(&mut canonical, partial_tree);
                canonical.extend_from_slice(conflict.id.to_string().as_bytes());
            }
        }
        Self {
            id: ApplicationId::of_canonical_bytes(&canonical),
            revision_id,
            target_view,
            result,
        }
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
        let mut canonical = b"wheat-view-v1\0".to_vec();
        append_tree(&mut canonical, &tree);
        for (change, revision) in &frontier {
            canonical.push(0xfe);
            canonical.extend_from_slice(change.to_string().as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(revision.to_string().as_bytes());
        }
        for (change, application) in &applications {
            canonical.push(0xfd);
            canonical.extend_from_slice(change.to_string().as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(application.to_string().as_bytes());
        }
        for conflict in &unresolved_conflicts {
            canonical.push(0xfc);
            canonical.extend_from_slice(conflict.to_string().as_bytes());
        }
        for content in &required_content {
            canonical.push(0xfb);
            canonical.extend_from_slice(content.to_string().as_bytes());
        }
        Self {
            id: ViewId::from_canonical_bytes(&canonical),
            tree,
            frontier,
            applications,
            unresolved_conflicts,
            required_content,
        }
    }
}

fn append_tree(canonical: &mut Vec<u8>, tree: &Tree) {
    for (path, content) in tree {
        canonical.push(0xff);
        canonical.extend_from_slice(path.as_bytes());
        canonical.push(0);
        canonical.extend_from_slice(content.to_string().as_bytes());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictResolution {
    pub conflict_revision: RevisionId,
    pub resolved_tree: Tree,
}
