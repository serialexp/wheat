//! `wheat blame PATH`: which change last wrote each line of a file in the effective
//! view, walked through the store's path history index (`wheat_store::blame`).

use std::{fmt::Write as _, num::NonZeroUsize, path::Path};

use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use wheat_core::{RepositoryStateRecord, ViewId, ViewRecord};
use wheat_store::{
    BlameChange, BlameError, BlameLimits, BlameOrigin, ChangeMapStore, ContentStore,
    ImmutableObjectStore, MerkleTreeStore, PathHistoryIndex, PublicationStore, RepositoryReader,
};

use crate::{
    authoring_metadata,
    local::{PATH_HISTORY_INDEX_FILE, open, runtime},
    outcome::{CliFailure, MAX_OUTPUT_BYTES, OutputFormat, Success},
};

/// The longest file blamed: its text is printed with a prefix per line.
pub const MAX_BLAME_BYTES: usize = 4 * 1024 * 1024;

/// What to blame.
pub struct BlamePath {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// Walk at most this many of the file's changes; lines older than them are left
    /// unresolved.
    pub limit: Option<NonZeroUsize>,
}

#[derive(Debug, Serialize)]
struct Origin {
    change_id: String,
    revision_id: String,
    position: u64,
    title: String,
    author: String,
    authored_at: String,
}

#[derive(Debug, Serialize)]
struct Range {
    start: u32,
    len: u32,
    /// The index of its origin, or none when the walk stopped before it.
    origin: Option<usize>,
    /// For an unresolved range, the view it carries on in and its first line there.
    #[serde(skip_serializing_if = "Option::is_none")]
    before_view: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    before_line: Option<u32>,
}

/// Blames `request.path` in `effective_view`.
pub fn inspect(
    root: &Path,
    effective_view: ViewId,
    request: BlamePath,
    format: OutputFormat,
) -> Result<Success> {
    let path = request.path.as_str();
    if path.is_empty()
        || path
            .split('/')
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(CliFailure::invalid(format!(
            "{path:?} is not a repository-relative file path"
        )));
    }
    let repository = open(root)?;
    let runtime = runtime()?;
    runtime.block_on(async {
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
        let view = reader.view(effective_view).await?;
        if view.id() != effective_view {
            return Err(CliFailure::corrupt("effective view identity mismatch"));
        }
        let index = PathHistoryIndex::open(&root.join(PATH_HISTORY_INDEX_FILE))?;
        let limits = BlameLimits {
            max_bytes: MAX_BLAME_BYTES,
            max_changes: request.limit.unwrap_or(NonZeroUsize::MAX),
        };
        let blame = match wheat_store::blame(&reader, &index, &view, path, limits).await {
            Ok(Some(blame)) => blame,
            Ok(None) => {
                return Err(CliFailure::invalid(format!(
                    "no file at {path:?} in the effective view"
                )));
            }
            Err(BlameError::NotText) => {
                return Err(CliFailure::invalid(format!("{path:?} is not text")));
            }
            Err(error @ BlameError::TooLong { .. }) => {
                return Err(CliFailure::limit(format!("{path:?}: {error}")));
            }
            Err(error) => return Err(error.into()),
        };
        let change_map = ChangeMapStore::new(&repository.objects);
        let origins = origins(&reader, &change_map, &state, &blame.origins).await?;
        let ranges: Vec<Range> = blame
            .ranges
            .iter()
            .map(|range| match range.origin {
                BlameOrigin::Change { index } => Range {
                    start: range.start,
                    len: range.len,
                    origin: Some(index),
                    before_view: None,
                    before_line: None,
                },
                BlameOrigin::Unresolved { before, line } => Range {
                    start: range.start,
                    len: range.len,
                    origin: None,
                    before_view: Some(before.to_string()),
                    before_line: Some(line),
                },
            })
            .collect();
        let complete = ranges.iter().all(|range| range.origin.is_some());
        let human = if format == OutputFormat::Human {
            let trees = MerkleTreeStore::new(&repository.objects);
            human(&trees, &repository.content, &view, path, &ranges, &origins).await?
        } else {
            String::new()
        };
        Success::new(
            "blame",
            json!({
                "path": path,
                "lines": blame.lines,
                "ranges": ranges,
                "origins": origins,
                "complete": complete,
            }),
            human,
        )
    })
}

/// The changes the ranges name, with their revisions' metadata.
async fn origins<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    change_map: &ChangeMapStore<'_, O>,
    state: &RepositoryStateRecord,
    changes: &[BlameChange],
) -> Result<Vec<Origin>> {
    let mut origins = Vec::with_capacity(changes.len());
    for origin in changes {
        let revision_id = origin.application.revision_id;
        let revision = reader.revision(revision_id).await?;
        if revision.id() != revision_id || revision.change_id != origin.touch.change {
            return Err(CliFailure::corrupt("revision identity mismatch"));
        }
        let change_object = change_map
            .get(state.changes, revision.change_id)
            .await?
            .ok_or_else(|| CliFailure::corrupt("revision refers to an unindexed logical change"))?;
        let change = reader.change(change_object).await?;
        if change.id != revision.change_id {
            return Err(CliFailure::corrupt("logical change identity mismatch"));
        }
        origins.push(Origin {
            change_id: change.id.to_string(),
            revision_id: revision_id.to_string(),
            position: origin.touch.position,
            title: change.metadata.title,
            author: revision.metadata.author.name,
            authored_at: authoring_metadata::display_timestamp(revision.metadata.authored_at)?,
        });
    }
    Ok(origins)
}

/// The file's lines, grouped under the change that wrote them.
async fn human<O: ImmutableObjectStore>(
    trees: &MerkleTreeStore<'_, O>,
    content: &ContentStore,
    view: &ViewRecord,
    path: &str,
    ranges: &[Range],
    origins: &[Origin],
) -> Result<String> {
    let components: Vec<String> = path.split('/').map(str::to_owned).collect();
    let value = trees
        .lookup(view.tree, &components)
        .await?
        .ok_or_else(|| CliFailure::corrupt("the blamed file left the view"))?;
    let bytes = content.get_bounded(value.content, MAX_BLAME_BYTES)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split_inclusive('\n');
    let width = ranges
        .last()
        .map_or(1, |range| (range.start + range.len).to_string().len());
    let mut output = String::new();
    for range in ranges {
        match range.origin {
            Some(index) => {
                let origin = &origins[index];
                writeln!(
                    output,
                    "{} {} {} {}",
                    origin.change_id, origin.author, origin.authored_at, origin.title
                )?;
            }
            None => writeln!(
                output,
                "older than the changes walked (raise --limit to resolve)"
            )?,
        }
        for number in range.start..range.start + range.len {
            let line = lines.next().unwrap_or_default();
            write!(output, "  {:>width$}  {line}", number + 1)?;
            if !line.ends_with('\n') {
                output.push('\n');
            }
        }
        if output.len() > MAX_OUTPUT_BYTES {
            return Err(CliFailure::limit("blame output exceeds the output limit"));
        }
    }
    Ok(output)
}
