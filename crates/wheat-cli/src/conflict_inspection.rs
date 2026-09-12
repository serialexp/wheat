//! Bounded expansion and atomic rendering of durable conflicts.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

use anyhow::Result;
use serde::Serialize;
use wheat_core::{EditFailure, EditId, FileValue, ObjectId, ViewId};
use wheat_store::{ConflictSetStore, PublicationStore, RepositoryReader, RetainedSetStore};

use crate::{command_output::CliFailure, local::LocalRepository};

const MAX_CONFLICTS: usize = 100_000;
const MAX_DISTINCT_EDITS: usize = 250_000;
const MAX_EDIT_OCCURRENCES: usize = 250_000;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_AGGREGATE_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct ConflictDocument {
    pub schema_version: u8,
    pub head_generation: u64,
    pub conflicts: Vec<ConflictEntry>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConflictEntry {
    pub id: wheat_core::ConflictId,
    pub base_view: ViewId,
    pub edits: Vec<ConflictEdit>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConflictEdit {
    pub id: EditId,
    pub path: String,
    pub target: Option<FileValue>,
    pub reason: EditFailure,
}

pub(crate) async fn inspect(repository: &LocalRepository) -> Result<ConflictDocument> {
    let head = repository.heads.read()?;
    let reader = RepositoryReader::new(&repository.objects, &repository.content);
    // Every typed read authenticates the immutable object's bytes against the requested ID.
    // Follow only the roots selected by this head: complete validation is deliberately reserved
    // for `wheat validate`, since walking unrelated retained history makes inspection unbounded.
    let envelope = reader.envelope(head.envelope).await?;
    if envelope.id() != head.envelope {
        return Err(CliFailure::corrupt("repository envelope identity mismatch"));
    }
    let state = reader.state(envelope.state).await?;
    if state.id() != envelope.state {
        return Err(CliFailure::corrupt("repository state identity mismatch"));
    }
    let retained = RetainedSetStore::new(&repository.objects);
    if !retained
        .contains(state.retained_views, ObjectId::from(state.working_view))
        .await?
    {
        return Err(CliFailure::corrupt(
            "working view is not retained by the current repository state",
        ));
    }
    let working = reader.view(state.working_view).await?;
    if working.id() != state.working_view {
        return Err(CliFailure::corrupt("working view identity mismatch"));
    }
    let conflicts = ConflictSetStore::new(&repository.objects);
    let mut cursor = conflicts.cursor(working.unresolved_conflicts).await?;
    let mut entries = Vec::new();
    let mut paths = BTreeMap::<EditId, String>::new();
    let mut validated_edits = BTreeSet::<EditId>::new();
    let mut retained_base_views = BTreeSet::<ViewId>::new();
    let mut edit_occurrences = 0usize;
    let mut aggregate_path_bytes = 0usize;
    let mut model_bytes = 0usize;
    while let Some(id) = cursor.next().await? {
        if entries.len() >= MAX_CONFLICTS {
            return Err(CliFailure::limit("conflict count exceeds inspection limit"));
        }
        let conflict = reader.conflict(id).await?;
        if conflict.id != id {
            return Err(CliFailure::corrupt("conflict identity mismatch"));
        }
        if retained_base_views.insert(conflict.base_view) {
            if !retained
                .contains(state.retained_views, ObjectId::from(conflict.base_view))
                .await?
            {
                return Err(CliFailure::corrupt(
                    "conflict base view is not retained by the current repository state",
                ));
            }
            let base_view = reader.view(conflict.base_view).await?;
            if base_view.id() != conflict.base_view {
                return Err(CliFailure::corrupt("conflict base view identity mismatch"));
            }
        }
        model_bytes = bounded_add(model_bytes, 64, MAX_MODEL_BYTES, "conflict model")?;
        let mut edits = Vec::new();
        for (edit_id, evidence) in conflict.edits {
            edit_occurrences = bounded_add(
                edit_occurrences,
                1,
                MAX_EDIT_OCCURRENCES,
                "conflict edit occurrence count",
            )?;
            let path = if let Some(path) = paths.get(&edit_id) {
                path.clone()
            } else {
                if paths.len() >= MAX_DISTINCT_EDITS {
                    return Err(CliFailure::limit(
                        "distinct conflict edit count exceeds inspection limit",
                    ));
                }
                let path = reader.edit(edit_id).await?.path;
                if path.len() > MAX_PATH_BYTES {
                    return Err(CliFailure::limit(
                        "conflict edit path exceeds inspection limit",
                    ));
                }
                aggregate_path_bytes = bounded_add(
                    aggregate_path_bytes,
                    path.len(),
                    MAX_AGGREGATE_PATH_BYTES,
                    "aggregate conflict path bytes",
                )?;
                paths.insert(edit_id, path.clone());
                path
            };
            aggregate_path_bytes = bounded_add(
                aggregate_path_bytes,
                path.len(),
                MAX_AGGREGATE_PATH_BYTES,
                "aggregate conflict path bytes",
            )?;
            // IDs, target, enum tag and interference IDs are all retained in the output model.
            let reason_bytes = match &evidence.reason {
                EditFailure::Interference { with } => {
                    if with.is_empty() {
                        return Err(CliFailure::corrupt("empty edit interference evidence"));
                    }
                    for referenced in with {
                        if validated_edits.insert(*referenced) {
                            if validated_edits.len() > MAX_DISTINCT_EDITS {
                                return Err(CliFailure::limit(
                                    "distinct conflict edit count exceeds inspection limit",
                                ));
                            }
                            reader.edit(*referenced).await?;
                        }
                    }
                    with.len().checked_mul(32)
                }
                _ => Some(0),
            }
            .ok_or_else(|| CliFailure::limit("conflict model size overflow"))?;
            if validated_edits.insert(edit_id) && validated_edits.len() > MAX_DISTINCT_EDITS {
                return Err(CliFailure::limit(
                    "distinct conflict edit count exceeds inspection limit",
                ));
            }
            model_bytes = bounded_add(
                model_bytes,
                32usize
                    .checked_add(path.len())
                    .and_then(|size| size.checked_add(34))
                    .and_then(|size| size.checked_add(reason_bytes))
                    .ok_or_else(|| CliFailure::limit("conflict model size overflow"))?,
                MAX_MODEL_BYTES,
                "conflict model",
            )?;
            edits.push(ConflictEdit {
                id: edit_id,
                path,
                target: evidence.target,
                reason: evidence.reason,
            });
        }
        edits.sort_by_key(|edit| edit.id);
        entries.push(ConflictEntry {
            id: conflict.id,
            base_view: conflict.base_view,
            edits,
        });
    }
    entries.sort_by_key(|conflict| conflict.id);
    Ok(ConflictDocument {
        schema_version: 1,
        head_generation: head.generation,
        conflicts: entries,
    })
}

fn bounded_add(current: usize, added: usize, limit: usize, name: &str) -> Result<usize> {
    let total = current
        .checked_add(added)
        .ok_or_else(|| CliFailure::limit(format!("{name} overflow")))?;
    if total > limit {
        return Err(CliFailure::limit(format!(
            "{name} exceeds inspection limit"
        )));
    }
    Ok(total)
}

pub(crate) fn human(document: &ConflictDocument, destination: &mut impl Write) -> Result<()> {
    atomic_render(destination, MAX_OUTPUT_BYTES, |output| {
        for conflict in &document.conflicts {
            writeln!(output, "conflict {} on {}", conflict.id, conflict.base_view)?;
            for edit in &conflict.edits {
                // Debug string formatting quotes and escapes newlines and control bytes.
                writeln!(
                    output,
                    "  edit {} path={:?} target={:?} reason={:?}",
                    edit.id, edit.path, edit.target, edit.reason
                )?;
            }
        }
        Ok(())
    })
}

#[cfg(test)]
fn json(document: &ConflictDocument, destination: &mut impl Write) -> Result<()> {
    atomic_render(destination, MAX_OUTPUT_BYTES, |output| {
        serde_json::to_writer_pretty(&mut *output, document)?;
        writeln!(output)?;
        Ok(())
    })
}

fn atomic_render(
    destination: &mut impl Write,
    limit: usize,
    render: impl FnOnce(&mut BoundedBuffer) -> Result<()>,
) -> Result<()> {
    let mut output = BoundedBuffer::new(limit);
    let result = render(&mut output);
    if output.limit_exceeded {
        return Err(CliFailure::limit(
            "conflict output exceeded configured byte limit",
        ));
    }
    result?;
    destination.write_all(&output.bytes)?;
    Ok(())
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    limit_exceeded: bool,
}

impl BoundedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            limit_exceeded: false,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(new_len) = self.bytes.len().checked_add(bytes.len()) else {
            self.limit_exceeded = true;
            return Err(io::Error::other("conflict output size overflow"));
        };
        if new_len > self.limit {
            self.limit_exceeded = true;
            return Err(io::Error::other(
                "conflict output exceeded configured byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_core::{ConflictId, ContentId, FileKind};

    fn document(path: &str) -> ConflictDocument {
        ConflictDocument {
            schema_version: 1,
            head_generation: 42,
            conflicts: vec![ConflictEntry {
                id: ConflictId::from_digest([1; 32]),
                base_view: ViewId::from_digest([2; 32]),
                edits: vec![ConflictEdit {
                    id: EditId::from_digest([3; 32]),
                    path: path.to_owned(),
                    target: Some(FileValue {
                        content: ContentId::from_digest([4; 32]),
                        kind: FileKind::Executable,
                    }),
                    reason: EditFailure::KindMismatch,
                }],
            }],
        }
    }

    #[test]
    fn aggregate_bounds_accept_the_limit_and_return_typed_failures() {
        assert_eq!(bounded_add(7, 3, 10, "test").unwrap(), 10);
        for error in [
            bounded_add(10, 1, 10, "test").unwrap_err(),
            bounded_add(usize::MAX, 1, usize::MAX, "test").unwrap_err(),
        ] {
            assert!(matches!(
                error.downcast_ref::<CliFailure>(),
                Some(CliFailure::LimitExceeded(_))
            ));
        }
    }

    #[test]
    fn human_paths_are_quoted_and_control_characters_are_escaped() {
        let mut output = Vec::new();
        human(&document("line\n\tname"), &mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("path=\"line\\n\\tname\""));
        assert!(!rendered.contains("path=line\n"));
    }

    #[test]
    fn json_contains_identifiers_target_and_no_fabricated_ranges() {
        let mut output = Vec::new();
        json(&document("path"), &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["head_generation"], 42);
        let conflict = &value["conflicts"][0];
        assert_eq!(conflict["id"], serde_json::to_value([1_u8; 32]).unwrap());
        assert_eq!(
            conflict["base_view"],
            serde_json::to_value([2_u8; 32]).unwrap()
        );
        let edit = &conflict["edits"][0];
        assert_eq!(edit["id"], serde_json::to_value([3_u8; 32]).unwrap());
        assert_eq!(edit["path"], "path");
        assert_eq!(
            edit["target"]["content"],
            serde_json::to_value([4_u8; 32]).unwrap()
        );
        assert_eq!(edit["target"]["kind"], "Executable");
        assert_eq!(edit["reason"], "KindMismatch");
        assert!(edit.get("range").is_none());
        assert!(edit.get("read").is_none());
        assert!(edit.get("write").is_none());
    }

    #[test]
    fn rendering_preserves_multiple_conflicts_edits_duplicate_paths_reasons_and_targets() {
        let first_edit = ConflictEdit {
            id: EditId::from_digest([10; 32]),
            path: "duplicate".to_owned(),
            target: None,
            reason: EditFailure::NoMatch,
        };
        let second_edit = ConflictEdit {
            id: EditId::from_digest([11; 32]),
            path: "duplicate".to_owned(),
            target: Some(FileValue {
                content: ContentId::from_digest([12; 32]),
                kind: FileKind::Symlink,
            }),
            reason: EditFailure::Interference {
                with: [EditId::from_digest([13; 32])].into_iter().collect(),
            },
        };
        let document = ConflictDocument {
            schema_version: 1,
            head_generation: 9,
            conflicts: vec![
                ConflictEntry {
                    id: ConflictId::from_digest([20; 32]),
                    base_view: ViewId::from_digest([21; 32]),
                    edits: vec![first_edit],
                },
                ConflictEntry {
                    id: ConflictId::from_digest([22; 32]),
                    base_view: ViewId::from_digest([23; 32]),
                    edits: vec![second_edit],
                },
            ],
        };

        let mut output = Vec::new();
        json(&document, &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["conflicts"].as_array().unwrap().len(), 2);
        assert_eq!(value["conflicts"][0]["edits"][0]["path"], "duplicate");
        assert_eq!(value["conflicts"][1]["edits"][0]["path"], "duplicate");
        assert!(value["conflicts"][0]["edits"][0]["target"].is_null());
        assert_eq!(value["conflicts"][0]["edits"][0]["reason"], "NoMatch");
        assert_eq!(
            value["conflicts"][1]["edits"][0]["target"]["kind"],
            "Symlink"
        );
        assert!(value["conflicts"][1]["edits"][0]["reason"]["Interference"]["with"].is_array());
    }

    #[test]
    fn clean_json_has_a_stable_empty_shape() {
        let clean = ConflictDocument {
            schema_version: 1,
            head_generation: 7,
            conflicts: Vec::new(),
        };
        let mut output = Vec::new();
        json(&clean, &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schema_version": 1,
                "head_generation": 7,
                "conflicts": [],
            })
        );
    }

    #[test]
    fn successful_render_is_committed_with_one_destination_write() {
        #[derive(Default)]
        struct CountingWriter {
            bytes: Vec<u8>,
            writes: usize,
        }
        impl Write for CountingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes += 1;
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut destination = CountingWriter::default();
        json(&document("path"), &mut destination).unwrap();
        assert_eq!(destination.writes, 1);
        assert!(!destination.bytes.is_empty());
    }

    #[test]
    fn output_limit_refuses_human_and_json_atomically() {
        let oversized = document(&"x".repeat(MAX_OUTPUT_BYTES));
        let mut human_destination = Vec::new();
        let human_error = human(&oversized, &mut human_destination).unwrap_err();
        assert!(matches!(
            human_error.downcast_ref::<CliFailure>(),
            Some(CliFailure::LimitExceeded(_))
        ));
        assert!(human_destination.is_empty());

        let mut json_destination = Vec::new();
        let json_error = json(&oversized, &mut json_destination).unwrap_err();
        assert!(matches!(
            json_error.downcast_ref::<CliFailure>(),
            Some(CliFailure::LimitExceeded(_))
        ));
        assert!(json_destination.is_empty());
    }
}
