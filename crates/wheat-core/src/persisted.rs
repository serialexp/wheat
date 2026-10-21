use crate::{
    ApplicationMapId, ChangeMapId, ChangeSetId, ConflictSetId, ContentMapId, ContentSetId,
    ProjectedCommitSetId, RepositoryStateId, RetainedSetId, RevisionMapId, TreeId, ViewId,
    ViewLineageMapId, ViewOrder,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewRecord {
    pub tree: TreeId,
    pub revisions: RevisionMapId,
    pub applications: ApplicationMapId,
    pub unresolved_conflicts: ConflictSetId,
    /// The blobs the tree names, each with the number of tree entries naming it.
    pub required_content: ContentMapId,
    /// The changes in `revisions` that no other change in `revisions` depends on: the
    /// dependencies of a new change created on the frontier.
    pub tips: ChangeSetId,
    pub order: ViewOrder,
}

impl ViewRecord {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::view_record_bytes(self)
    }
    #[must_use]
    pub fn id(&self) -> ViewId {
        ViewId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryStateRecord {
    pub base_view: ViewId,
    pub changes: ChangeMapId,
    pub view_lineage: ViewLineageMapId,
    /// Every blob a retained tree, edit or conflict names.
    pub required_content: ContentSetId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
    /// The Git commit each pushed application projects as. Every member's application
    /// is in `retained_applications`.
    pub projected_commits: ProjectedCommitSetId,
}

impl RepositoryStateRecord {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::repository_state_record_bytes(self)
    }
    #[must_use]
    pub fn id(&self) -> RepositoryStateId {
        RepositoryStateId::of_canonical_bytes(&self.canonical_bytes())
    }
}
