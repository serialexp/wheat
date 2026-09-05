use thiserror::Error;
use wheat_core::{ObjectId, Repository, RepositoryClosure};

use crate::{
    ContentStore, ContentStoreError, ImmutableObjectStore, ObjectStoreError, PublicationError,
    PublicationStore, RepositoryHead,
};

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
    Applications(#[from] crate::ApplicationMapError),
    #[error(transparent)]
    Tree(#[from] crate::MerkleTreeError),
}

pub struct RepositoryPublisher<'a, O, P> {
    objects: &'a O,
    content: &'a ContentStore,
    publications: &'a P,
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
        for object in &closure.objects {
            self.objects.put_if_absent(object.id, &object.bytes).await?;
        }
        self.verify_closure(&closure).await?;
        Ok(closure)
    }

    pub async fn initialize(
        &self,
        repository: &Repository,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let closure = self.prepare(repository).await?;
        let head = RepositoryHead::initial(closure.state.id.into());
        self.publications.initialize(head)?;
        Ok(head)
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
        crate::RepositoryReader::new(self.objects, self.content)
            .validate_complete(root)
            .await?;
        let head = RepositoryHead::initial(root.into());
        self.publications.initialize(head)?;
        Ok(head)
    }

    pub async fn publish_root(
        &self,
        expected: RepositoryHead,
        root: wheat_core::RepositoryStateId,
    ) -> Result<RepositoryHead, RepositoryPublicationError> {
        let proposed = expected.next(root.into())?;
        let actual = self.publications.read()?;
        if actual != expected {
            return Err(PublicationError::Stale { expected, actual }.into());
        }
        let reader = crate::RepositoryReader::new(self.objects, self.content);
        reader.validate_complete(root).await?;
        reader
            .validate_transition(
                wheat_core::RepositoryStateId::from_digest(*expected.repository_state.as_bytes()),
                root,
            )
            .await?;
        self.publications.compare_and_advance(expected, proposed)?;
        Ok(proposed)
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
