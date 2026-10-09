//! Joining two repository envelopes that grew apart from shared history, as a fetch
//! does when the local repository holds work the remote lacks.
//!
//! Every part of a repository state only grows, so the union of two states holds both
//! sides' history and is a valid successor of each. Each part is joined by comparing
//! the two roots, which skips the subtrees they share: the cost follows how far the
//! sides moved apart, not how much history they hold. The content manifest is the
//! exception; it is one flat set, rewritten whole the way every mutation rewrites it.
//!
//! Labels live outside the state. They are joined three ways against the manifest
//! both sides started from.

use thiserror::Error;
use wheat_core::{
    ChangeId, ContentManifestRecord, RepositoryEnvelopeId, RepositoryEnvelopeRecord,
    RepositoryStateId, RepositoryStateRecord, ViewId, ViewLabelManifestRecord,
};

use crate::{
    ChangeMapError, ChangeMapStore, CollectionError, ContentSetStore, ImmutableObjectStore,
    ObjectStoreError, RepositoryPublicationError, RepositoryReader, RetainedSetStore,
    ViewLineageError, ViewLineageMapStore,
};

#[derive(Debug, Error)]
pub enum RepositoryUnionError {
    #[error("the repositories start from different base views ({local} and {remote})")]
    DifferentBaseView { local: ViewId, remote: ViewId },
    #[error(
        "{} changes were recorded differently on each side: {}",
        .0.len(),
        .0.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
    )]
    DivergentChanges(Vec<ChangeId>),
    #[error("labels were moved differently on each side: {}", .0.join(", "))]
    DivergentLabels(Vec<String>),
    #[error(transparent)]
    Repository(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Object(#[from] ObjectStoreError),
    #[error(transparent)]
    Collection(#[from] CollectionError),
    #[error(transparent)]
    Changes(ChangeMapError),
    #[error(transparent)]
    Lineage(#[from] ViewLineageError),
}

impl From<ChangeMapError> for RepositoryUnionError {
    fn from(error: ChangeMapError) -> Self {
        match error {
            ChangeMapError::DivergentEntries(changes) => Self::DivergentChanges(changes),
            other => Self::Changes(other),
        }
    }
}

type Result<T> = std::result::Result<T, RepositoryUnionError>;

/// Joins label manifests three ways. A label one side left as it was in `base` takes
/// the other side's value, including its removal; a label both sides changed the same
/// way keeps that change. A label both sides changed differently is a conflict, and
/// all of them are returned, sorted by name.
pub fn merge_labels(
    base: &ViewLabelManifestRecord,
    local: &ViewLabelManifestRecord,
    remote: &ViewLabelManifestRecord,
) -> std::result::Result<ViewLabelManifestRecord, Vec<String>> {
    let names: std::collections::BTreeSet<&String> =
        local.labels.keys().chain(remote.labels.keys()).collect();
    let mut merged = std::collections::BTreeMap::new();
    let mut divergent = Vec::new();
    for name in names {
        let was = base.labels.get(name);
        let here = local.labels.get(name);
        let there = remote.labels.get(name);
        let label = if here == there || there == was {
            here
        } else if here == was {
            there
        } else {
            divergent.push(name.clone());
            continue;
        };
        if let Some(label) = label {
            merged.insert(name.clone(), label.clone());
        }
    }
    if divergent.is_empty() {
        Ok(ViewLabelManifestRecord { labels: merged })
    } else {
        Err(divergent)
    }
}

impl<O: ImmutableObjectStore> RepositoryReader<'_, O> {
    /// The state holding all of `local` and all of `remote`, written to the store.
    ///
    /// Both must start from the same base view. A change recorded with a different
    /// change object on each side is refused: one change identity never names two.
    #[tracing::instrument(name = "union_states", level = "debug", skip_all)]
    pub async fn union_states(
        &self,
        local: RepositoryStateId,
        remote: RepositoryStateId,
    ) -> Result<RepositoryStateId> {
        if local == remote {
            return Ok(local);
        }
        let here = self.state(local).await?;
        let there = self.state(remote).await?;
        if here.base_view != there.base_view {
            return Err(RepositoryUnionError::DifferentBaseView {
                local: here.base_view,
                remote: there.base_view,
            });
        }

        let changes = ChangeMapStore::new(self.objects)
            .union(here.changes, there.changes)
            .await?;
        let view_lineage = ViewLineageMapStore::new(self.objects)
            .union(here.view_lineage, there.view_lineage)
            .await?;
        let retained = RetainedSetStore::new(self.objects);
        let retained_changes = retained
            .union(here.retained_changes, there.retained_changes)
            .await?;
        let retained_views = retained
            .union(here.retained_views, there.retained_views)
            .await?;
        let retained_revisions = retained
            .union(here.retained_revisions, there.retained_revisions)
            .await?;
        let retained_applications = retained
            .union(here.retained_applications, there.retained_applications)
            .await?;

        let required_content = if here.required_content == there.required_content {
            here.required_content
        } else {
            let here_content = self.manifest(here.required_content).await?.content;
            let there_content = self.manifest(there.required_content).await?.content;
            let content = ContentSetStore::new(self.objects)
                .union(here_content, there_content)
                .await?;
            let manifest = ContentManifestRecord { content };
            self.objects
                .put_if_absent(manifest.id().into(), &manifest.canonical_bytes())
                .await?;
            manifest.id()
        };

        let state = RepositoryStateRecord {
            base_view: here.base_view,
            changes,
            view_lineage,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
        };
        self.objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await?;
        Ok(state.id())
    }

    /// The envelope joining `local` and `remote`: the union of their states, and their
    /// labels merged against `base`, the envelope both sides last agreed on. Without a
    /// `base` every label starts out absent, so a label both sides hold must agree.
    #[tracing::instrument(name = "union_envelopes", level = "debug", skip_all)]
    pub async fn union_envelopes(
        &self,
        base: Option<RepositoryEnvelopeId>,
        local: RepositoryEnvelopeId,
        remote: RepositoryEnvelopeId,
    ) -> Result<RepositoryEnvelopeId> {
        if local == remote || Some(local) == base {
            return Ok(remote);
        }
        if Some(remote) == base {
            return Ok(local);
        }
        let here = self.envelope(local).await?;
        let there = self.envelope(remote).await?;
        let base_labels = match base {
            Some(base) => {
                let base = self.envelope(base).await?;
                self.view_label_manifest(base.labels).await?
            }
            None => ViewLabelManifestRecord {
                labels: std::collections::BTreeMap::new(),
            },
        };
        let labels = merge_labels(
            &base_labels,
            &self.view_label_manifest(here.labels).await?,
            &self.view_label_manifest(there.labels).await?,
        )
        .map_err(RepositoryUnionError::DivergentLabels)?;
        self.objects
            .put_if_absent(labels.id().into(), &labels.canonical_bytes())
            .await?;
        let envelope = RepositoryEnvelopeRecord {
            state: self.union_states(here.state, there.state).await?,
            labels: labels.id(),
        };
        self.objects
            .put_if_absent(envelope.id().into(), &envelope.canonical_bytes())
            .await?;
        Ok(envelope.id())
    }
}
