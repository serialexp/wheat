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
    #[error("required content {0} is missing")]
    MissingContent(wheat_core::ContentId),
    #[error("published immutable closure is incomplete at object {0}")]
    MissingObject(ObjectId),
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
        repository
            .validate()
            .expect("repository publication requires valid model state");
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
        let proposed = expected.next(closure.state.id.into())?;
        self.verify_closure(&closure).await?;
        self.publications.compare_and_advance(expected, proposed)?;
        Ok(proposed)
    }

    async fn verify_closure(
        &self,
        closure: &RepositoryClosure,
    ) -> Result<(), RepositoryPublicationError> {
        let ids = closure
            .objects
            .iter()
            .map(|object| object.id)
            .collect::<Vec<_>>();
        for (id, exists) in ids.iter().zip(self.objects.exists_many(&ids).await?) {
            if !exists {
                return Err(RepositoryPublicationError::MissingObject(*id));
            }
        }
        for content in &closure.required_content {
            if !self.content.contains(*content) {
                return Err(RepositoryPublicationError::MissingContent(*content));
            }
            self.content.get(*content)?;
        }
        Ok(())
    }
}
