use crate::{
    ApplicationMapId, ConflictSetId, ContentManifestId, ContentSetId, RepositoryStateId,
    RetainedSetId, RevisionMapId, TreeId, ViewId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ViewRecord {
    pub tree: TreeId,
    pub revisions: RevisionMapId,
    pub applications: ApplicationMapId,
    pub unresolved_conflicts: ConflictSetId,
    pub required_content: ContentManifestId,
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
pub struct ContentManifestRecord {
    pub content: ContentSetId,
}

impl ContentManifestRecord {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::content_manifest_record_bytes(self)
    }
    #[must_use]
    pub fn id(&self) -> ContentManifestId {
        ContentManifestId::of_canonical_bytes(&self.canonical_bytes())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryStateRecord {
    pub base_view: ViewId,
    pub working_view: ViewId,
    pub published_view: ViewId,
    pub required_content: ContentManifestId,
    pub retained_changes: RetainedSetId,
    pub retained_views: RetainedSetId,
    pub retained_revisions: RetainedSetId,
    pub retained_applications: RetainedSetId,
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
