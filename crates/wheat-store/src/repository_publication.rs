use thiserror::Error;
use wheat_core::{ObjectId, Repository, RepositoryClosure};

use crate::{
    ContentStore, ContentStoreError, ImmutableObjectStore, ObjectStoreError, PublicationError,
    PublicationStore, RepositoryHead,
};

/// Concurrent writes when [`RepositoryPublisher::prepare`] stores a closure, for
/// backends that write objects one by one.
const PREPARE_WRITE_CONCURRENCY: usize = 16;

#[derive(Debug, Error)]
pub enum RepositoryPublicationError {
    #[error("immutable object publication failed")]
    Object(#[from] ObjectStoreError),
    #[error("required content publication failed")]
    Content(#[from] ContentStoreError),
    #[error("repository head publication failed")]
    Head(#[from] PublicationError),
    #[error("repository model is invalid")]
    InvalidRepository(#[from] wheat_core::RepositoryError),
    #[error("required content {0} is missing")]
    MissingContent(wheat_core::ContentId),
    #[error("published immutable closure is incomplete at object {0}")]
    MissingObject(ObjectId),
    #[error("invalid persisted references: {0}")]
    InvalidReferences(&'static str),
    #[error(transparent)]
    Collection(#[from] crate::CollectionError),
    #[error(transparent)]
    Revisions(#[from] crate::RevisionMapError),
    #[error(transparent)]
    Changes(#[from] crate::ChangeMapError),
    #[error(transparent)]
    Applications(#[from] crate::ApplicationMapError),
    #[error(transparent)]
    Tree(#[from] crate::MerkleTreeError),
    #[error(transparent)]
    VerificationScratch(#[from] crate::verification_scratch::VerificationScratchError),
}

pub struct RepositoryPublisher<'a, O, P> {
    pub(crate) objects: &'a O,
    pub(crate) content: &'a ContentStore,
    pub(crate) publications: &'a P,
}

impl<'a, O, P> RepositoryPublisher<'a, O, P>
where
    O: ImmutableObjectStore,
    P: PublicationStore,
{
    #[must_use]
    pub fn new(objects: &'a O, content: &'a ContentStore, publications: &'a P) -> Self {
        Self {
            objects,
            content,
            publications,
        }
    }

    pub async fn prepare(
        &self,
        repository: &Repository,
    ) -> Result<RepositoryClosure, RepositoryPublicationError> {
        repository.validate()?;
        let closure = repository.canonical_closure();
        let objects = closure
            .objects
            .iter()
            .map(|object| (object.id, object.bytes.clone()))
            .collect::<Vec<_>>();
        self.objects
            .put_batch(&objects, PREPARE_WRITE_CONCURRENCY)
            .await?;
        self.verify_closure(&closure).await?;
        Ok(closure)
    }

    pub async fn initialize(
        &self,
        repository: &Repository,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let closure = self.prepare(repository).await?;
        self.initialize_root(closure.state.id).await
    }

    pub async fn publish(
        &self,
        expected: RepositoryHead,
        repository: &Repository,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let closure = self.prepare(repository).await?;
        self.publish_root(expected, closure.state.id).await
    }

    pub async fn initialize_root(
        &self,
        root: wheat_core::RepositoryStateId,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let labels = wheat_core::ViewLabelManifestRecord {
            labels: Default::default(),
        };
        let envelope = self.prepare_envelope(root, &labels).await?;
        self.initialize_prepared_envelope(envelope).await
    }

    pub async fn publish_root(
        &self,
        expected: RepositoryHead,
        root: wheat_core::RepositoryStateId,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let reader = crate::RepositoryReader::new(self.objects, self.content);
        let old = reader.envelope(expected.envelope).await?;
        let labels = reader.view_label_manifest(old.labels).await?;
        let envelope = self.prepare_envelope(root, &labels).await?;
        self.publish_envelope(expected, envelope).await
    }

    async fn verify_closure(
        &self,
        closure: &RepositoryClosure,
    ) -> Result<(), RepositoryPublicationError> {
        crate::RepositoryReader::new(self.objects, self.content)
            .validate_complete(closure.state.id)
            .await
    }
}
