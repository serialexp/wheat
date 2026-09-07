//! Publication metadata is separate from the executable reference repository.
use crate::{
    ImmutableObjectStore, PublicationError, PublicationStore, RepositoryHead,
    RepositoryPublicationError, RepositoryPublisher, RepositoryReader, RetainedSetStore,
};
use wheat_core::{
    BranchManifestRecord, RepositoryEnvelopeId, RepositoryEnvelopeRecord, RepositoryStateId,
};
type Result<T> = std::result::Result<T, RepositoryPublicationError>;
impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    pub async fn validate_envelope(
        &self,
        id: RepositoryEnvelopeId,
    ) -> Result<RepositoryEnvelopeRecord> {
        let envelope = self.envelope(id).await?;
        self.validate_complete(envelope.state).await?;
        let state = self.state(envelope.state).await?;
        let branches = self.branch_manifest(envelope.branches).await?;
        let retained = RetainedSetStore::new(self.objects);
        for target in branches.branches.values() {
            if !retained
                .contains(state.retained_views, (*target).into())
                .await?
            {
                return Err(RepositoryPublicationError::InvalidReferences(
                    "branch target not retained",
                ));
            }
            self.view(*target).await?;
        }
        Ok(envelope)
    }
}
impl<O: ImmutableObjectStore, P: PublicationStore> RepositoryPublisher<'_, O, P> {
    pub async fn prepare_envelope(
        &self,
        state: RepositoryStateId,
        branches: &BranchManifestRecord,
    ) -> Result<RepositoryEnvelopeId> {
        self.objects
            .put_if_absent(branches.id().into(), &branches.canonical_bytes())
            .await?;
        let envelope = RepositoryEnvelopeRecord {
            state,
            branches: branches.id(),
        };
        self.objects
            .put_if_absent(envelope.id().into(), &envelope.canonical_bytes())
            .await?;
        RepositoryReader::new(self.objects, self.content)
            .validate_envelope(envelope.id())
            .await?;
        Ok(envelope.id())
    }
    /// Initialize a head for an envelope previously returned by `prepare_envelope`.
    ///
    /// The caller must not mutate the immutable stores between preparation and
    /// this call. The publication store only receives the already authenticated ID.
    pub async fn initialize_prepared_envelope(
        &self,
        root: RepositoryEnvelopeId,
    ) -> Result<RepositoryHead> {
        let head = RepositoryHead::initial(root);
        self.publications.initialize(head)?;
        Ok(head)
    }

    pub async fn initialize_envelope(&self, root: RepositoryEnvelopeId) -> Result<RepositoryHead> {
        RepositoryReader::new(self.objects, self.content)
            .validate_envelope(root)
            .await?;
        self.initialize_prepared_envelope(root).await
    }
    /// Explicit branch-manifest publication. Ordinary model publication preserves
    /// the authenticated old manifest through `publish_root` instead.
    pub async fn publish_envelope(
        &self,
        expected: RepositoryHead,
        root: RepositoryEnvelopeId,
    ) -> Result<RepositoryHead> {
        let actual = self.publications.read()?;
        if actual != expected {
            return Err(PublicationError::Stale { expected, actual }.into());
        }
        let reader = RepositoryReader::new(self.objects, self.content);
        let new = reader.validate_envelope(root).await?;
        let old = reader.envelope(expected.envelope).await?;
        reader.validate_transition(old.state, new.state).await?;
        let proposed = expected.next(root)?;
        self.publications.compare_and_advance(expected, proposed)?;
        Ok(proposed)
    }
}
