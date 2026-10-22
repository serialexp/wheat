//! Immutable repository comparison resolution and bounded Merkle traversal.
//!
//! Presentation deliberately lives in the shared diff model/renderer seam. This module only
//! resolves user-facing selectors against one validated repository-head snapshot and produces
//! neutral tree changes.

use std::str::FromStr;

use anyhow::Result;
use wheat_core::{FileValue, ObjectId, RevisionId, TreeId, ViewId};
use wheat_store::{
    ContentStoreError, MerkleTreeError, MerkleTreeStore, PublicationStore, RepositoryReader,
    RetainedSetStore, TreeDiffLimits, TreePathFilter,
};

use crate::{local::LocalRepository, outcome::CliFailure};

const MAX_DIFF_CHANGES: usize = 250_000;
const MAX_DIFF_PATH_BYTES: usize = 4_096;
const MAX_INLINE_CONTENT_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_INLINE_CONTENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeChangeInput {
    pub path: String,
    pub old: Option<TreeSideInput>,
    pub new: Option<TreeSideInput>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeSideInput {
    pub value: FileValue,
    /// Present only when content fits both the per-side and command aggregate inline budgets.
    pub bytes: Option<Vec<u8>>,
    /// Exact when `bytes` is present; otherwise one more than the rejected bounded-read limit.
    pub min_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ComparisonInput {
    Revision {
        revision: RevisionId,
        from: TreeId,
        to: TreeId,
    },
    Views {
        from_view: ViewId,
        to_view: ViewId,
        from: TreeId,
        to: TreeId,
    },
}

pub struct RepositoryDiffInput {
    pub head_generation: u64,
    pub comparison: ComparisonInput,
    pub changes: Vec<TreeChangeInput>,
}

pub async fn revision(
    repository: &LocalRepository,
    revision_id: RevisionId,
    filter: &TreePathFilter,
    deadline: std::time::Instant,
) -> Result<RepositoryDiffInput> {
    let snapshot = Snapshot::read(repository).await?;
    if !snapshot
        .retained
        .contains(
            snapshot.state.retained_revisions,
            ObjectId::from(revision_id),
        )
        .await?
    {
        return Err(CliFailure::invalid(
            "revision is not retained by the current repository state",
        ));
    }
    let revision = snapshot.reader.revision(revision_id).await?;
    if revision.id() != revision_id {
        return Err(CliFailure::corrupt("revision identity mismatch"));
    }
    let from = if let Some(predecessor_id) = revision.predecessor {
        if !snapshot
            .retained
            .contains(
                snapshot.state.retained_revisions,
                ObjectId::from(predecessor_id),
            )
            .await?
        {
            return Err(CliFailure::corrupt(
                "revision predecessor is not retained by the current repository state",
            ));
        }
        let predecessor = snapshot.reader.revision(predecessor_id).await?;
        if predecessor.id() != predecessor_id {
            return Err(CliFailure::corrupt(
                "revision predecessor identity mismatch",
            ));
        }
        predecessor.resulting_tree
    } else {
        snapshot
            .resolve_retained_view(revision.authored_against)
            .await?
            .tree
    };
    finish(
        repository,
        snapshot.head_generation,
        ComparisonInput::Revision {
            revision: revision_id,
            from,
            to: revision.resulting_tree,
        },
        from,
        revision.resulting_tree,
        filter,
        deadline,
    )
    .await
}

pub async fn views(
    repository: &LocalRepository,
    from: &str,
    to: &str,
    filter: &TreePathFilter,
    deadline: std::time::Instant,
) -> Result<RepositoryDiffInput> {
    let snapshot = Snapshot::read(repository).await?;
    let (from_view, from_record) = snapshot.resolve_view(from).await?;
    let (to_view, to_record) = snapshot.resolve_view(to).await?;
    finish(
        repository,
        snapshot.head_generation,
        ComparisonInput::Views {
            from_view,
            to_view,
            from: from_record.tree,
            to: to_record.tree,
        },
        from_record.tree,
        to_record.tree,
        filter,
        deadline,
    )
    .await
}

/// Diffs `from` against `to`, reporting and reading content only for the paths
/// `filter` selects.
async fn finish(
    repository: &LocalRepository,
    head_generation: u64,
    comparison: ComparisonInput,
    from: TreeId,
    to: TreeId,
    filter: &TreePathFilter,
    _deadline: std::time::Instant,
) -> Result<RepositoryDiffInput> {
    let trees = MerkleTreeStore::new(&repository.objects);
    let mut cursor = trees
        .diff_cursor(
            from,
            to,
            TreeDiffLimits {
                max_changes: MAX_DIFF_CHANGES,
                max_path_bytes: MAX_DIFF_PATH_BYTES,
                ..TreeDiffLimits::default()
            },
        )
        .restricted_to(filter.clone());
    let mut changes = Vec::new();
    let mut remaining_inline_bytes = MAX_TOTAL_INLINE_CONTENT_BYTES;
    while let Some(change) = cursor.next().await.map_err(classify_diff_error)? {
        let old_value = change.old;
        let new_value = change.new;
        let old = read_side(repository, old_value, &mut remaining_inline_bytes)?;
        let new = if old_value.as_ref().map(|value| value.content)
            == new_value.as_ref().map(|value| value.content)
        {
            new_value.map(|value| reuse_side(value, old.as_ref()))
        } else {
            read_side(repository, new_value, &mut remaining_inline_bytes)?
        };
        changes.push(TreeChangeInput {
            path: change.path,
            old,
            new,
        });
    }
    Ok(RepositoryDiffInput {
        head_generation,
        comparison,
        changes,
    })
}

fn classify_diff_error(error: MerkleTreeError) -> anyhow::Error {
    match error {
        error @ (MerkleTreeError::ChangeLimitExceeded { .. }
        | MerkleTreeError::NodeLimitExceeded { .. }
        | MerkleTreeError::EntryLimitExceeded { .. }
        | MerkleTreeError::PathBytesExceeded { .. }
        | MerkleTreeError::DepthExceeded { .. }
        | MerkleTreeError::NodeBytesExceeded { .. }
        | MerkleTreeError::BufferedEntriesExceeded { .. }) => CliFailure::limit(error.to_string()),
        error => error.into(),
    }
}

fn reuse_side(value: FileValue, source: Option<&TreeSideInput>) -> TreeSideInput {
    let source = source.expect("equal content identity has an old side");
    TreeSideInput {
        value,
        bytes: source.bytes.clone(),
        min_bytes: source.min_bytes,
    }
}

fn read_side(
    repository: &LocalRepository,
    value: Option<FileValue>,
    remaining: &mut usize,
) -> Result<Option<TreeSideInput>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let allowance = (*remaining).min(MAX_INLINE_CONTENT_BYTES);
    let (bytes, min_bytes) = match repository.content.get_bounded(value.content, allowance) {
        Ok(bytes) => {
            *remaining = remaining.checked_sub(bytes.len()).ok_or_else(|| {
                CliFailure::corrupt("repository diff inline byte budget underflow")
            })?;
            let len = bytes.len();
            (Some(bytes), len)
        }
        Err(ContentStoreError::TooLarge { minimum, .. }) => (None, minimum),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(TreeSideInput {
        value,
        bytes,
        min_bytes,
    }))
}

struct Snapshot<'a> {
    reader: RepositoryReader<'a, wheat_store::PackedObjectStore>,
    retained: RetainedSetStore<'a, wheat_store::PackedObjectStore>,
    head_generation: u64,
    envelope: wheat_core::RepositoryEnvelopeRecord,
    state: wheat_core::RepositoryStateRecord,
}

impl<'a> Snapshot<'a> {
    async fn read(repository: &'a LocalRepository) -> Result<Self> {
        let head = repository.heads.read()?;
        let reader = RepositoryReader::new(&repository.objects, &repository.content);
        let envelope = reader.envelope(head.envelope).await?;
        if envelope.id() != head.envelope {
            return Err(CliFailure::corrupt("repository envelope identity mismatch"));
        }
        let state = reader.state(envelope.state).await?;
        if state.id() != envelope.state {
            return Err(CliFailure::corrupt("repository state identity mismatch"));
        }
        // Diff follows only the requested retained roots. Complete history validation belongs to
        // `wheat validate`; doing it here would make a sparse comparison scale with all history.
        Ok(Self {
            reader,
            retained: RetainedSetStore::new(&repository.objects),
            head_generation: head.generation,
            envelope,
            state,
        })
    }

    async fn resolve_view(&self, selector: &str) -> Result<(ViewId, wheat_core::ViewRecord)> {
        let id = match selector {
            _ if selector.starts_with("label:") => {
                let name = selector.strip_prefix("label:").expect("prefix checked");
                if name.is_empty() {
                    return Err(CliFailure::invalid("label selector requires a name"));
                }
                self.reader
                    .view_label_manifest(self.envelope.labels)
                    .await?
                    .labels
                    .get(name)
                    .map(|label| label.view)
                    .ok_or_else(|| CliFailure::invalid(format!("unknown label {name}")))?
            }
            _ => ViewId::from_str(selector)
                .map_err(|_| CliFailure::invalid("invalid view selector"))?,
        };
        if !self
            .retained
            .contains(self.state.retained_views, ObjectId::from(id))
            .await?
        {
            return Err(CliFailure::invalid(
                "view is not retained by the current repository state",
            ));
        }
        let view = self.reader.view(id).await?;
        if view.id() != id {
            return Err(CliFailure::corrupt("view identity mismatch"));
        }
        Ok((id, view))
    }

    async fn resolve_retained_view(&self, id: ViewId) -> Result<wheat_core::ViewRecord> {
        if !self
            .retained
            .contains(self.state.retained_views, ObjectId::from(id))
            .await?
        {
            return Err(CliFailure::corrupt(
                "view is not retained by the current repository state",
            ));
        }
        let view = self.reader.view(id).await?;
        if view.id() != id {
            return Err(CliFailure::corrupt("view identity mismatch"));
        }
        Ok(view)
    }
}
