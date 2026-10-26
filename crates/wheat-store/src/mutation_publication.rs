//! Two-phase publication for persisted repository mutations.
//!
//! Mutation metadata can be prepared into a buffered object store, made durable by the caller,
//! and only then installed with an authoritative compare-and-advance operation. A label may
//! move in the same publication ([`LabelMove`]), so the mutation and the move land together
//! or not at all.

use wheat_core::{
    RepositoryEnvelopeRecord, RepositoryStateId, ViewId, ViewLabelManifestId,
    ViewLabelManifestRecord,
};

use crate::{
    ContentStore, ImmutableObjectStore, PreparedRepositoryMutation, PublicationError,
    PublicationStore, RepositoryHead, RepositoryMutationError, RepositoryPublicationError,
    RepositoryReader, RetainedSetStore,
};

/// A label moving from one view to another in the publication of a mutation. The move
/// keeps the label's annotation and provenance. Whether it is forward, and what it
/// leaves behind, the caller checks before preparing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelMove {
    pub name: String,
    /// The view the label must name in the head the mutation was prepared against.
    pub from: ViewId,
    /// A view the mutation's state retains.
    pub to: ViewId,
}

/// `labels` with `label_move` applied, stored; refused when the label does not name
/// `from` there or `state` does not retain `to`.
pub(crate) async fn labels_after<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    labels: ViewLabelManifestId,
    state: RepositoryStateId,
    label_move: Option<&LabelMove>,
) -> Result<ViewLabelManifestId, RepositoryMutationError> {
    let Some(label_move) = label_move else {
        return Ok(labels);
    };
    let reader = RepositoryReader::new(objects, content);
    let mut manifest = reader.view_label_manifest(labels).await?;
    let actual = manifest
        .labels
        .get(&label_move.name)
        .map(|label| label.view);
    let Some(label) = manifest
        .labels
        .get_mut(&label_move.name)
        .filter(|label| label.view == label_move.from)
    else {
        return Err(RepositoryMutationError::LabelMoved {
            name: label_move.name.clone(),
            expected: label_move.from,
            actual,
        });
    };
    label.view = label_move.to;
    let state = reader.state(state).await?;
    if !RetainedSetStore::new(objects)
        .contains(state.retained_views, label_move.to.into())
        .await?
    {
        return Err(RepositoryPublicationError::InvalidReferences(
            "a label moves to a view the mutation does not retain",
        )
        .into());
    }
    let manifest =
        ViewLabelManifestRecord::new(manifest.labels).map_err(crate::ObjectStoreError::from)?;
    objects
        .put_if_absent(manifest.id().into(), &manifest.canonical_bytes())
        .await?;
    Ok(manifest.id())
}

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
pub async fn prepare_mutation_publication<O: ImmutableObjectStore, P: PublicationStore>(
    objects: &O,
    content: &ContentStore,
    publications: &P,
    expected: RepositoryHead,
    mutation: PreparedRepositoryMutation,
) -> Result<PreparedMutationPublication, RepositoryMutationError> {
    prepare_mutation_publication_moving_label(
        objects,
        content,
        publications,
        expected,
        mutation,
        None,
    )
    .await
}

/// [`prepare_mutation_publication`], moving `label_move`'s label in the same envelope.
#[tracing::instrument(level = "debug", skip_all)]
pub async fn prepare_mutation_publication_moving_label<
    O: ImmutableObjectStore,
    P: PublicationStore,
>(
    objects: &O,
    content: &ContentStore,
    publications: &P,
    expected: RepositoryHead,
    mutation: PreparedRepositoryMutation,
    label_move: Option<&LabelMove>,
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
        labels: labels_after(objects, content, old.labels, mutation.state(), label_move).await?,
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
