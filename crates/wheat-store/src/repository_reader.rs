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
        view_label_manifest,
        ViewLabelManifestId,
        ViewLabelManifestRecord,
        ViewLabelManifest,
        decode_view_label_manifest_record
    );
    read_record!(
        envelope,
        RepositoryEnvelopeId,
        RepositoryEnvelopeRecord,
        RepositoryEnvelope,
        decode_repository_envelope_record
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

    /// `view`'s producing application: the application of its last change. None for the
    /// base.
    pub(crate) async fn producing_application(
        &self,
        view: &ViewRecord,
    ) -> Result<Option<ApplicationId>> {
        let Some(last) = view.order.last else {
            return Ok(None);
        };
        crate::ApplicationMapStore::new(self.objects)
            .get(view.applications, last)
            .await?
            .map(Some)
            .ok_or(RepositoryPublicationError::InvalidReferences(
                "view lacks its last change's application",
            ))
    }

    /// The order index of `change` applied onto `target`: O(log n) application reads
    /// along `target`'s composition.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn application_order(
        &self,
        target: &ViewRecord,
        change: ChangeId,
    ) -> Result<ApplicationOrder> {
        let previous = self.producing_application(target).await?;
        let mut order = ApplicationOrder::begin(change, target.order.size, previous);
        while let Some(id) = order.wanted() {
            order
                .extend(&self.application(id).await?.order)
                .map_err(|_| order_index_mismatch())?;
        }
        Ok(order)
    }

    /// The application at canonical `position` of `view`'s composition, none when
    /// `view` has no change there: O(log n) application reads along the skip chain.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn application_at(
        &self,
        view: &ViewRecord,
        position: u64,
    ) -> Result<Option<(ApplicationId, ApplicationRecord)>> {
        if position >= view.order.size {
            return Ok(None);
        }
        let mut next = self.producing_application(view).await?;
        while let Some(id) = next {
            let record = self.application(id).await?;
            let Some(gap) = record.order.position.checked_sub(position) else {
                return Err(order_index_mismatch());
            };
            if gap == 0 {
                return Ok(Some((id, record)));
            }
            next = Some(
                *record
                    .order
                    .skip
                    .get(gap.ilog2() as usize)
                    .ok_or_else(order_index_mismatch)?,
            );
        }
        Err(order_index_mismatch())
    }

    /// The highest ChangeId at `view`'s canonical positions `from` onwards, none when
    /// there are none: O(log n) application reads.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn highest_change_from(
        &self,
        view: &ViewRecord,
        from: u64,
    ) -> Result<Option<ChangeId>> {
        let mut query = RangeMax::new(from, view.order.size);
        if query.is_empty() {
            return Ok(None);
        }
        let mut next = self.producing_application(view).await?;
        while let Some(id) = next {
            next = query
                .fold(&self.application(id).await?.order)
                .map_err(|_| order_index_mismatch())?;
        }
        Ok(query.highest())
    }
}

fn order_index_mismatch() -> RepositoryPublicationError {
    RepositoryPublicationError::InvalidReferences(
        "application order index does not line up with the applications it names",
    )
}
