//! Joining two repository envelopes that grew apart from shared history, as a fetch
//! does when the local repository holds work the remote lacks.
//!
//! Every part of a repository state only grows, so the union of two states holds both
//! sides' history and is a valid successor of each. Each part is joined by comparing
//! the two roots, which skips the subtrees they share: the cost follows how far the
//! sides moved apart, not how much history they hold.
//!
//! Labels live outside the state. They are joined three ways against the manifest
//! both sides started from, under a [`LabelPolicy`]: a fetch settles a label both sides
//! moved by which move is forward, a publish refuses it.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::{
    ChangeId, RepositoryEnvelopeId, RepositoryEnvelopeRecord, RepositoryStateId,
    RepositoryStateRecord, ViewId, ViewLabel, ViewLabelManifestRecord,
};

use crate::{
    ChangeMapError, ChangeMapStore, CollectionError, ContentSetStore, ImmutableObjectStore,
    MergeBudget, ObjectStoreError, ProjectedCommitSetStore, RepositoryPublicationError,
    RepositoryReader, RetainedSetStore, ViewLineageError, ViewLineageMapStore,
};

/// How a label both sides changed differently joins.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LabelPolicy {
    /// A fetch. A label both sides moved to different views, each keeping its
    /// annotation and provenance as they were, takes the remote's view when the move
    /// from the local one to it is forward, else the local one's when the move the
    /// other way is, else the remote's. Any other label both sides changed
    /// differently refuses.
    Fetch,
    /// A publish. Any label both sides changed differently refuses: the publisher
    /// fetches and integrates first.
    Publish,
}

/// Labels merged three ways.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelMerge {
    pub labels: ViewLabelManifestRecord,
    /// Under [`LabelPolicy::Fetch`], the labels both sides moved to different views,
    /// sorted by name. `labels` holds the remote's value for each; which side's view
    /// it takes is the caller's to settle.
    pub both_moved: Vec<String>,
}

/// A label whose value in a joined envelope differs from the local side's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelChange {
    pub name: String,
    /// None when the local side lacks it.
    pub local: Option<ViewLabel>,
    /// None when the joined envelope lacks it.
    pub joined: Option<ViewLabel>,
}

/// An envelope joining two sides, and its labels that differ from the local side's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvelopeUnion {
    pub envelope: RepositoryEnvelopeId,
    /// Sorted by name.
    pub label_changes: Vec<LabelChange>,
}

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
    #[error(
        "the joined repository would have {0} labels, more than the {max} a repository may hold",
        max = wheat_core::MAX_LABELS
    )]
    TooManyLabels(usize),
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
/// way keeps that change. A label both sides changed differently joins as `policy`
/// says; those that conflict are all returned, sorted by name.
pub fn merge_labels(
    base: &ViewLabelManifestRecord,
    local: &ViewLabelManifestRecord,
    remote: &ViewLabelManifestRecord,
    policy: LabelPolicy,
) -> std::result::Result<LabelMerge, Vec<String>> {
    let names: BTreeSet<&String> = local.labels.keys().chain(remote.labels.keys()).collect();
    let mut merged = BTreeMap::new();
    let mut both_moved = Vec::new();
    let mut divergent = Vec::new();
    for name in names {
        let was = base.labels.get(name);
        let here = local.labels.get(name);
        let there = remote.labels.get(name);
        let label = if here == there || there == was {
            here
        } else if here == was {
            there
        } else if policy == LabelPolicy::Fetch
            && let (Some(was), Some(here), Some(there)) = (was, here, there)
            && only_moved(was, here)
            && only_moved(was, there)
        {
            both_moved.push(name.clone());
            Some(there)
        } else {
            divergent.push(name.clone());
            continue;
        };
        if let Some(label) = label {
            merged.insert(name.clone(), label.clone());
        }
    }
    if divergent.is_empty() {
        Ok(LabelMerge {
            labels: ViewLabelManifestRecord { labels: merged },
            both_moved,
        })
    } else {
        Err(divergent)
    }
}

/// Whether `now` is `was` naming another view, its annotation and provenance as they
/// were.
fn only_moved(was: &ViewLabel, now: &ViewLabel) -> bool {
    now.view != was.view
        && now.original_git_ref == was.original_git_ref
        && now.annotation == was.annotation
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

        let required_content = ContentSetStore::new(self.objects)
            .union(here.required_content, there.required_content)
            .await?;
        // Each side's members name its own retained applications, so the union's
        // name the union's.
        let projected_commits = ProjectedCommitSetStore::new(self.objects)
            .union(here.projected_commits, there.projected_commits)
            .await?;

        let state = RepositoryStateRecord {
            base_view: here.base_view,
            changes,
            view_lineage,
            required_content,
            retained_changes,
            retained_views,
            retained_revisions,
            retained_applications,
            projected_commits,
        };
        self.objects
            .put_if_absent(state.id().into(), &state.canonical_bytes())
            .await?;
        Ok(state.id())
    }

    /// The envelope joining `local` and `remote` under [`LabelPolicy::Publish`]; see
    /// [`Self::union_envelopes_under`].
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
        Ok(self
            .merge_envelopes(base, local, remote, LabelPolicy::Publish)
            .await?
            .envelope)
    }

    /// The envelope joining `local` and `remote`: the union of their states, and their
    /// labels merged against `base`, the envelope both sides last agreed on, under
    /// `policy`. Without a `base` every label starts out absent, so a label both sides
    /// hold must agree. A label both sides moved is settled with
    /// [`RepositoryReader::forward`], every such check of one join sharing one
    /// [`MergeBudget`]. Returns it with the labels that differ from `local`'s.
    #[tracing::instrument(name = "union_envelopes", level = "debug", skip_all)]
    pub async fn union_envelopes_under(
        &self,
        base: Option<RepositoryEnvelopeId>,
        local: RepositoryEnvelopeId,
        remote: RepositoryEnvelopeId,
        policy: LabelPolicy,
    ) -> Result<EnvelopeUnion> {
        if local == remote || Some(local) == base {
            return self.joined(local, remote).await;
        }
        if Some(remote) == base {
            return self.joined(local, local).await;
        }
        self.merge_envelopes(base, local, remote, policy).await
    }

    /// The union of two envelopes neither of which is `base`.
    async fn merge_envelopes(
        &self,
        base: Option<RepositoryEnvelopeId>,
        local: RepositoryEnvelopeId,
        remote: RepositoryEnvelopeId,
        policy: LabelPolicy,
    ) -> Result<EnvelopeUnion> {
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
        let local_labels = self.view_label_manifest(here.labels).await?;
        let merge = merge_labels(
            &base_labels,
            &local_labels,
            &self.view_label_manifest(there.labels).await?,
            policy,
        )
        .map_err(RepositoryUnionError::DivergentLabels)?;
        let mut labels = merge.labels;
        let mut budget = MergeBudget::STANDARD;
        for name in &merge.both_moved {
            let local_view = self.view(local_labels.labels[name].view).await?;
            let remote_view = self.view(labels.labels[name].view).await?;
            if self
                .forward(&local_view, &remote_view, &mut budget)
                .await?
                .is_some()
                && self
                    .forward(&remote_view, &local_view, &mut budget)
                    .await?
                    .is_none()
            {
                labels
                    .labels
                    .insert(name.clone(), local_labels.labels[name].clone());
            }
        }
        if labels.labels.len() > wheat_core::MAX_LABELS {
            return Err(RepositoryUnionError::TooManyLabels(labels.labels.len()));
        }
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
        Ok(EnvelopeUnion {
            envelope: envelope.id(),
            label_changes: changed_labels(&local_labels, &labels),
        })
    }

    /// `joined`, which is `local` or the remote, with its labels that differ from
    /// `local`'s.
    async fn joined(
        &self,
        local: RepositoryEnvelopeId,
        joined: RepositoryEnvelopeId,
    ) -> Result<EnvelopeUnion> {
        let mut label_changes = Vec::new();
        if joined != local {
            let (here, there) = (self.envelope(local).await?, self.envelope(joined).await?);
            if here.labels != there.labels {
                label_changes = changed_labels(
                    &self.view_label_manifest(here.labels).await?,
                    &self.view_label_manifest(there.labels).await?,
                );
            }
        }
        Ok(EnvelopeUnion {
            envelope: joined,
            label_changes,
        })
    }
}

/// The labels of `joined` whose value differs from `local`'s, sorted by name.
fn changed_labels(
    local: &ViewLabelManifestRecord,
    joined: &ViewLabelManifestRecord,
) -> Vec<LabelChange> {
    let names: BTreeSet<&String> = local.labels.keys().chain(joined.labels.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let (here, there) = (local.labels.get(name), joined.labels.get(name));
            (here != there).then(|| LabelChange {
                name: name.clone(),
                local: here.cloned(),
                joined: there.cloned(),
            })
        })
        .collect()
}
