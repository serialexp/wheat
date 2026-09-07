//! Lazy typed reads of canonical repository records; no hydrated Repository is needed.
use crate::{
    ContentStore, ImmutableObjectStore, ObjectKind, ObjectStoreError, RepositoryPublicationError,
};
use wheat_core::*;

type Result<T> = std::result::Result<T, RepositoryPublicationError>;

pub struct RepositoryReader<'a, O> {
    pub(crate) objects: &'a O,
    pub(crate) content: &'a ContentStore,
}

macro_rules! read_record {
    ($name:ident,$id:ty,$record:ty,$kind:ident,$decode:ident) => {
        pub async fn $name(&self, id: $id) -> Result<$record> {
            let object = self.objects.get(id.into()).await?;
            if object.kind != ObjectKind::$kind {
                return Err(ObjectStoreError::WrongKind {
                    id: id.into(),
                    expected: ObjectKind::$kind,
                    actual: object.kind,
                }
                .into());
            }
            $decode(&object.canonical_bytes).map_err(|e| ObjectStoreError::InvalidFormat(e).into())
        }
    };
}

impl<'a, O: ImmutableObjectStore> RepositoryReader<'a, O> {
    pub fn new(objects: &'a O, content: &'a ContentStore) -> Self {
        Self { objects, content }
    }
    read_record!(
        state,
        RepositoryStateId,
        RepositoryStateRecord,
        RepositoryState,
        decode_repository_state_record
    );
    read_record!(view, ViewId, ViewRecord, View, decode_view_record);
    read_record!(
        branch_manifest,
        BranchManifestId,
        BranchManifestRecord,
        BranchManifest,
        decode_branch_manifest_record
    );
    read_record!(
        envelope,
        RepositoryEnvelopeId,
        RepositoryEnvelopeRecord,
        RepositoryEnvelope,
        decode_repository_envelope_record
    );
    read_record!(
        manifest,
        ContentManifestId,
        ContentManifestRecord,
        ContentManifest,
        decode_content_manifest_record
    );
    read_record!(
        revision,
        RevisionId,
        RevisionRecord,
        Revision,
        decode_revision_record
    );
    read_record!(
        application,
        ApplicationId,
        ApplicationRecord,
        Application,
        decode_application_record
    );
    read_record!(change, ChangeObjectId, Change, Change, decode_change);
    read_record!(edit, EditId, EditRecord, Edit, decode_edit_record);
    read_record!(conflict, ConflictId, Conflict, Conflict, decode_conflict);
}
