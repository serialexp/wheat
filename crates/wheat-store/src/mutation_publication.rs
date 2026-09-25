//! Two-phase publication for persisted repository mutations.
//!
//! Mutation metadata can be prepared into a buffered object store, made durable by the caller,
//! and only then installed with an authoritative compare-and-advance operation.

use wheat_core::RepositoryEnvelopeRecord;

use crate::{
    ContentStore, ImmutableObjectStore, PreparedRepositoryMutation, PublicationError,
    PublicationStore, RepositoryHead, RepositoryMutationError, RepositoryPublicationError,
    RepositoryReader,
};

/// A sealed repository-head transition whose envelope has already been persisted.
///
/// Values can only be produced by [`prepare_mutation_publication`]. Keeping the transition sealed
/// prevents callers from finalizing an arbitrary head without first writing its envelope.
pub struct PreparedMutationPublication {
    expected: RepositoryHead,
    proposed: RepositoryHead,
    _sealed: (),
}

impl PreparedMutationPublication {
    /// The head against which this publication was prepared.
    #[must_use]
    pub fn expected(&self) -> RepositoryHead {
        self.expected
    }

    /// The head which will be installed if finalization succeeds.
    #[must_use]
    pub fn proposed(&self) -> RepositoryHead {
        self.proposed
    }
}

/// Persist the final envelope for a prepared mutation without advancing the repository head.
///
/// `objects` may be a buffering store. The caller is responsible for making its writes durable
/// before passing the returned value to [`finalize_mutation_publication`]. The current head is
/// checked before the old envelope is read, matching the stale-error behavior of ordinary
/// mutation publication.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prepare_mutation_publication<O: ImmutableObjectStore, P: PublicationStore>(
    objects: &O,
    content: &ContentStore,
    publications: &P,
    expected: RepositoryHead,
    mutation: PreparedRepositoryMutation,
) -> Result<PreparedMutationPublication, RepositoryMutationError> {
    let actual = publications
        .read()
        .map_err(RepositoryPublicationError::from)?;
    if actual != expected {
        return Err(RepositoryMutationError::Stale { expected, actual });
    }

    let old = RepositoryReader::new(objects, content)
        .envelope(expected.envelope)
        .await?;
    let envelope = RepositoryEnvelopeRecord {
        state: mutation.state(),
        labels: old.labels,
    };
    objects
        .put_if_absent(envelope.id().into(), &envelope.canonical_bytes())
        .await?;
    let proposed = expected
        .next(envelope.id())
        .map_err(RepositoryPublicationError::from)?;

    Ok(PreparedMutationPublication {
        expected,
        proposed,
        _sealed: (),
    })
}

/// Atomically install a previously prepared mutation envelope.
///
/// This compare-and-advance is the authoritative concurrency check. On failure the publication is
/// left unchanged; immutable objects prepared by the losing writer may safely remain stored.
#[tracing::instrument(level = "debug", skip_all)]
pub fn finalize_mutation_publication<P: PublicationStore>(
    publications: &P,
    prepared: PreparedMutationPublication,
) -> Result<RepositoryHead, RepositoryMutationError> {
    match publications.compare_and_advance(prepared.expected, prepared.proposed) {
        Ok(()) => Ok(prepared.proposed),
        Err(PublicationError::Stale { expected, actual }) => {
            Err(RepositoryMutationError::Stale { expected, actual })
        }
        Err(error) => Err(RepositoryPublicationError::from(error).into()),
    }
}
