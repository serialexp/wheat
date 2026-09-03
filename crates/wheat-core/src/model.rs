use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{ChangeId, ContentId, RevisionId, ViewId};

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
    pub base_view: ViewId,
    pub paths: Vec<PathConflict>,
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
    pub revision_id: RevisionId,
    pub target_view: ViewId,
    pub result: ApplicationResult,
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
}

impl RepositoryView {
    #[must_use]
    pub fn new(tree: Tree, frontier: BTreeMap<ChangeId, RevisionId>) -> Self {
        let mut canonical = Vec::new();
        for (path, content) in &tree {
            canonical.extend_from_slice(path.as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(content.to_string().as_bytes());
            canonical.push(0xff);
        }
        for (change, revision) in &frontier {
            canonical.extend_from_slice(change.to_string().as_bytes());
            canonical.push(0);
            canonical.extend_from_slice(revision.to_string().as_bytes());
            canonical.push(0xfe);
        }
        Self {
            id: ViewId::from_canonical_bytes(&canonical),
            tree,
            frontier,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictResolution {
    pub conflict_revision: RevisionId,
    pub resolved_tree: Tree,
}
