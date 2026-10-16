use std::{fmt::Write as _, path::Path};

use anyhow::Result;
use serde::Serialize;
use serde_json::json;
use wheat_core::{Change, ChangeMetadata, RevisionId, RevisionMetadata, RevisionRecord, ViewId};
use wheat_store::{PublicationStore, RepositoryReader, RevisionMapStore};

use crate::{
    authoring_metadata,
    command_output::{CliFailure, OutputFormat, Success},
    local::{open, runtime},
};

const MAX_LOG_HUMAN_BYTES: usize = crate::command_output::MAX_OUTPUT_BYTES;
// Leave room for the stable command envelope added by `emit_success`.
const MAX_LOG_JSON_RESULT_BYTES: usize = crate::command_output::MAX_OUTPUT_BYTES - 128;

#[derive(Debug, Serialize)]
struct LogEntry {
    change_id: String,
    change: ChangeMetadata,
    revision_id: String,
    predecessor: Option<String>,
    revision: RevisionMetadata,
    dependencies: Vec<String>,
    authored_against: String,
    resulting_tree: String,
}

#[derive(Debug)]
struct BoundedLogText {
    text: String,
    limit_exceeded: bool,
}

impl BoundedLogText {
    fn new() -> Self {
        Self {
            text: String::new(),
            limit_exceeded: false,
        }
    }

    fn finish(self) -> Result<String> {
        if self.limit_exceeded {
            return Err(CliFailure::limit("log output exceeds the output limit"));
        }
        Ok(self.text)
    }
}

impl std::fmt::Write for BoundedLogText {
    fn write_str(&mut self, value: &str) -> std::fmt::Result {
        let Some(new_len) = self.text.len().checked_add(value.len()) else {
            self.limit_exceeded = true;
            return Err(std::fmt::Error);
        };
        if new_len > MAX_LOG_HUMAN_BYTES {
            self.limit_exceeded = true;
            return Err(std::fmt::Error);
        }
        self.text.push_str(value);
        Ok(())
    }
}

pub(crate) fn inspect(
    root: &Path,
    effective_view: ViewId,
    format: OutputFormat,
) -> Result<Success> {
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
        let working = reader.view(effective_view).await?;
        if working.id() != effective_view {
            return Err(CliFailure::corrupt("effective view identity mismatch"));
        }
        let revisions = RevisionMapStore::new(&repository.objects);
        let changes = wheat_store::ChangeMapStore::new(&repository.objects);
        let mut cursor = revisions.cursor(working.revisions).await?;
        let mut output = BoundedLogText::new();
        let mut entries = Vec::new();
        let mut serialized_bytes = br#"{"entries":[]}"#.len();
        let mut record_count = 0usize;
        while let Some((change_id, revision_id)) = cursor.next().await? {
            record_count = record_count
                .checked_add(1)
                .ok_or_else(|| CliFailure::limit("log record count overflow"))?;
            if record_count > 250_000 {
                return Err(CliFailure::limit(
                    "current revision count exceeds the log limit",
                ));
            }
            let revision = reader.revision(revision_id).await?;
            if revision.id() != revision_id || revision.change_id != change_id {
                return Err(CliFailure::corrupt("revision identity mismatch"));
            }
            let change_object = changes
                .get(state.changes, change_id)
                .await?
                .ok_or_else(|| {
                    CliFailure::corrupt("revision refers to an unindexed logical change")
                })?;
            let change = reader.change(change_object).await?;
            if change.id != change_id {
                return Err(CliFailure::corrupt("logical change identity mismatch"));
            }
            match format {
                OutputFormat::Human => {
                    render_log_human_record(&mut output, &change, revision_id, &revision)?
                }
                OutputFormat::Json => push_log_entry(
                    &mut entries,
                    &mut serialized_bytes,
                    change,
                    revision_id,
                    revision,
                )?,
            }
        }
        let human = if format == OutputFormat::Human {
            output.finish()?
        } else {
            String::new()
        };
        Success::new("log", json!({ "entries": entries }), human)
    })
}

fn render_log_human_record(
    output: &mut BoundedLogText,
    change_record: &Change,
    revision: RevisionId,
    record: &RevisionRecord,
) -> Result<()> {
    let render = (|| -> std::fmt::Result {
        writeln!(
            output,
            "{} {revision} {}",
            change_record.id, change_record.metadata.title
        )?;
        writeln!(
            output,
            "  author {} <{}> {}",
            record.metadata.author.name,
            record.metadata.author.email,
            authoring_metadata::display_timestamp(record.metadata.authored_at)
                .map_err(|_| std::fmt::Error)?
        )?;
        if !record.metadata.message.is_empty() {
            write!(output, "  ")?;
            let mut remaining = record.metadata.message.as_str();
            while let Some(newline) = remaining.find('\n') {
                let (line, tail) = remaining.split_at(newline + 1);
                write!(output, "{line}  ")?;
                remaining = tail;
            }
            writeln!(output, "{remaining}")?;
        }
        Ok(())
    })();
    if output.limit_exceeded {
        return Err(CliFailure::limit("log output exceeds the output limit"));
    }
    render?;
    Ok(())
}

#[cfg(test)]
fn render_log_human<'a>(
    records: impl IntoIterator<Item = &'a (Change, RevisionId, RevisionRecord)>,
) -> Result<String> {
    let mut output = BoundedLogText::new();
    for (change, revision, record) in records {
        render_log_human_record(&mut output, change, *revision, record)?;
    }
    output.finish()
}

fn push_log_entry(
    entries: &mut Vec<LogEntry>,
    serialized_bytes: &mut usize,
    change_record: Change,
    revision: RevisionId,
    record: RevisionRecord,
) -> Result<()> {
    let entry = LogEntry {
        change_id: change_record.id.to_string(),
        change: change_record.metadata,
        revision_id: revision.to_string(),
        predecessor: record.predecessor.map(|id| id.to_string()),
        revision: record.metadata,
        dependencies: record
            .dependencies
            .iter()
            .map(ToString::to_string)
            .collect(),
        authored_against: record.authored_against.to_string(),
        resulting_tree: record.resulting_tree.to_string(),
    };
    let separator_bytes = usize::from(!entries.is_empty());
    let remaining = MAX_LOG_JSON_RESULT_BYTES
        .checked_sub(*serialized_bytes)
        .and_then(|bytes| bytes.checked_sub(separator_bytes))
        .ok_or_else(|| CliFailure::limit("log output exceeds the output limit"))?;
    let entry_bytes = bounded_json_size(&entry, remaining)?;
    *serialized_bytes = serialized_bytes
        .checked_add(separator_bytes)
        .and_then(|size| size.checked_add(entry_bytes))
        .ok_or_else(|| CliFailure::limit("log model size overflow"))?;
    entries.push(entry);
    Ok(())
}

#[cfg(test)]
fn build_log_entries(
    records: impl IntoIterator<Item = (Change, RevisionId, RevisionRecord)>,
) -> Result<Vec<LogEntry>> {
    let mut entries = Vec::new();
    let mut serialized_bytes = br#"{"entries":[]}"#.len();
    for (change, revision, record) in records {
        push_log_entry(
            &mut entries,
            &mut serialized_bytes,
            change,
            revision,
            record,
        )?;
    }
    Ok(entries)
}

struct JsonSizeCounter {
    bytes: usize,
    limit: usize,
    limit_exceeded: bool,
}

impl std::io::Write for JsonSizeCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(total) = self.bytes.checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(std::io::Error::other("log JSON size overflow"));
        };
        if total > self.limit {
            self.limit_exceeded = true;
            return Err(std::io::Error::other("log JSON exceeds the output limit"));
        }
        self.bytes = total;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn bounded_json_size(value: &impl Serialize, limit: usize) -> Result<usize> {
    let mut counter = JsonSizeCounter {
        bytes: 0,
        limit,
        limit_exceeded: false,
    };
    let rendered = serde_json::to_writer(&mut counter, value);
    if counter.limit_exceeded {
        return Err(CliFailure::limit("log output exceeds the output limit"));
    }
    rendered?;
    Ok(counter.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_core::{Actor, ChangeId, Timestamp, TreeId, ViewId};

    fn log_record(message: String) -> (Change, RevisionId, RevisionRecord) {
        let actor = Actor {
            name: "Test Author".to_owned(),
            email: "test@example.invalid".to_owned(),
        };
        (
            Change {
                id: ChangeId::from_bytes([1; 16]),
                metadata: ChangeMetadata {
                    title: "title".to_owned(),
                    description: String::new(),
                    creator: actor.clone(),
                    created_at: Timestamp::default(),
                    provenance: None,
                },
            },
            RevisionId::from_digest([2; 32]),
            RevisionRecord {
                change_id: ChangeId::from_bytes([1; 16]),
                metadata: RevisionMetadata {
                    message,
                    author: actor.clone(),
                    authored_at: Timestamp::default(),
                    committer: actor,
                    committed_at: Timestamp::default(),
                    provenance: None,
                },
                predecessor: None,
                combines: Default::default(),
                generation: 0,
                dependencies: Default::default(),
                authored_against: ViewId::from_digest([3; 32]),
                edits: Default::default(),
                transition: Default::default(),
                resulting_tree: TreeId::from_digest([4; 32]),
            },
        )
    }

    #[test]
    fn log_human_rendering_refuses_oversized_output_as_a_typed_limit() {
        let records = [log_record("x".repeat(MAX_LOG_HUMAN_BYTES))];
        let error = render_log_human(&records).expect_err("oversized log must be refused");
        assert!(matches!(
            error.downcast_ref::<CliFailure>(),
            Some(CliFailure::LimitExceeded(_))
        ));
    }

    #[test]
    fn log_json_model_refuses_oversized_output_as_a_typed_limit() {
        let records = [log_record("x".repeat(MAX_LOG_JSON_RESULT_BYTES))];
        let error = build_log_entries(records).expect_err("oversized log must be refused");
        assert!(matches!(
            error.downcast_ref::<CliFailure>(),
            Some(CliFailure::LimitExceeded(_))
        ));
    }
}
