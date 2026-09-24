//! Bounded expansion and atomic rendering of durable conflicts.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

use anyhow::Result;
use serde::Serialize;
use wheat_core::{
    ApplicationId, ApplicationResultRecord, ChangeId, EditFailure, EditId, FileValue, ObjectId,
    RevisionId, ViewId,
};
use wheat_store::{
    ApplicationMapStore, ConflictSetStore, ContentStore, ImmutableObjectStore, PublicationStore,
    RepositoryReader, RetainedSetStore, RevisionMapStore,
};

use crate::{command_output::CliFailure, local::LocalRepository};

pub(crate) const MAX_CONFLICTS: usize = 100_000;
const MAX_CURRENT_APPLICATIONS: usize = 250_000;
pub(crate) const MAX_DISTINCT_EDITS: usize = 250_000;
pub(crate) const MAX_EDIT_OCCURRENCES: usize = 250_000;
pub(crate) const MAX_PATH_BYTES: usize = 4_096;
const MAX_AGGREGATE_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

type CurrentApplication = (ChangeId, RevisionId, ApplicationId, ViewId);

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
    pub change_id: ChangeId,
    pub revision_id: RevisionId,
    pub application_id: ApplicationId,
    pub edits: Vec<ConflictEdit>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ConflictEdit {
    pub id: EditId,
    pub path: String,
    pub target: Option<FileValue>,
    pub reason: EditFailure,
}

pub(crate) async fn inspect(
    repository: &LocalRepository,
    effective_view: ViewId,
) -> Result<ConflictDocument> {
    inspect_store(
        &repository.objects,
        &repository.content,
        &repository.heads,
        effective_view,
    )
    .await
}

async fn inspect_store<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    heads: &impl PublicationStore,
    effective_view: ViewId,
) -> Result<ConflictDocument> {
    let head = heads.read()?;
    let reader = RepositoryReader::new(objects, content);
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
    let retained = RetainedSetStore::new(objects);
    if !retained
        .contains(state.retained_views, ObjectId::from(effective_view))
        .await?
    {
        return Err(CliFailure::corrupt(
            "effective view is not retained by the current repository state",
        ));
    }
    let working = reader.view(effective_view).await?;
    if working.id() != effective_view {
        return Err(CliFailure::corrupt("effective view identity mismatch"));
    }
    let applications = ApplicationMapStore::new(objects);
    let mut application_cursor = applications.cursor(working.applications).await?;
    let mut conflicted_applications = BTreeMap::<wheat_core::ConflictId, CurrentApplication>::new();
    let mut application_count = 0usize;
    while let Some((change_id, application_id)) = application_cursor.next().await? {
        application_count = bounded_add(
            application_count,
            1,
            MAX_CURRENT_APPLICATIONS,
            "current application count",
        )?;
        let application = reader.application(application_id).await?;
        let current_revision = RevisionMapStore::new(objects)
            .get(working.revisions, change_id)
            .await?;
        if current_revision != Some(application.revision_id) {
            return Err(CliFailure::corrupt(
                "current application does not refer to the current revision",
            ));
        }
        if let ApplicationResultRecord::Conflicted { conflict, .. } = application.result
            && conflicted_applications
                .insert(
                    conflict,
                    (
                        change_id,
                        application.revision_id,
                        application_id,
                        application.target_view,
                    ),
                )
                .is_some()
        {
            return Err(CliFailure::corrupt(
                "conflict is referenced by multiple current applications",
            ));
        }
    }
    let conflicts = ConflictSetStore::new(objects);
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
        let (change_id, revision_id, application_id) = correlate_conflict(
            &mut conflicted_applications,
            conflict.id,
            conflict.base_view,
        )?;
        entries.push(ConflictEntry {
            id: conflict.id,
            base_view: conflict.base_view,
            change_id,
            revision_id,
            application_id,
            edits,
        });
    }
    if !conflicted_applications.is_empty() {
        return Err(CliFailure::corrupt(
            "current conflicted application is absent from the unresolved conflict set",
        ));
    }
    entries.sort_by_key(|conflict| conflict.id);
    Ok(ConflictDocument {
        schema_version: 1,
        head_generation: head.generation,
        conflicts: entries,
    })
}

fn correlate_conflict(
    applications: &mut BTreeMap<wheat_core::ConflictId, CurrentApplication>,
    conflict_id: wheat_core::ConflictId,
    base_view: ViewId,
) -> Result<(ChangeId, RevisionId, ApplicationId)> {
    let (change_id, revision_id, application_id, target_view) =
        applications.remove(&conflict_id).ok_or_else(|| {
            CliFailure::corrupt("unresolved conflict has no current conflicted application")
        })?;
    if base_view != target_view {
        return Err(CliFailure::corrupt(
            "conflict base view does not match its current application's target view",
        ));
    }
    Ok((change_id, revision_id, application_id))
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
                change_id: ChangeId::from_bytes([5; 16]),
                revision_id: RevisionId::from_digest([6; 32]),
                application_id: ApplicationId::from_digest([7; 32]),
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

    /// Refuses to read one object, standing in for damaged history.
    struct Unreadable {
        inner: wheat_store::PackedObjectStore,
        refused: ObjectId,
    }

    #[async_trait::async_trait]
    impl ImmutableObjectStore for Unreadable {
        async fn put_if_absent(
            &self,
            id: ObjectId,
            bytes: &[u8],
        ) -> Result<(), wheat_store::ObjectStoreError> {
            self.inner.put_if_absent(id, bytes).await
        }

        async fn get(
            &self,
            id: ObjectId,
        ) -> Result<wheat_store::StoredObject, wheat_store::ObjectStoreError> {
            if id == self.refused {
                return Err(wheat_store::ObjectStoreError::Missing(id));
            }
            self.inner.get(id).await
        }

        async fn exists_many(
            &self,
            ids: &[ObjectId],
        ) -> Result<Vec<bool>, wheat_store::ObjectStoreError> {
            self.inner.exists_many(ids).await
        }
    }

    #[tokio::test]
    async fn conflict_inspection_does_not_read_unselected_retained_history() {
        let root = tempfile::tempdir().unwrap();
        let objects = wheat_store::PackedObjectStore::open(root.path().join("storage")).unwrap();
        let content = objects.content_store();
        let heads = wheat_store::LocalPublicationStore::new(root.path().join("repository.head"));
        content.put(b"value").unwrap();
        let record = wheat_core::EditRecord::new(
            "file",
            wheat_core::EditOperation::ExactFile {
                expected: None,
                replacement: Some(FileValue {
                    content: ContentId::of(b"value"),
                    kind: FileKind::Regular,
                }),
            },
        )
        .unwrap();
        let id = record.id();
        let mut repository = wheat_core::Repository::new(wheat_core::Tree::new());
        let base = repository.base_view().id;
        let (_, outcome) = repository
            .create_change(
                base,
                base,
                [],
                wheat_core::RevisionEdits {
                    edits: [(id, record)].into(),
                    transition: wheat_core::EditTransition {
                        added: [id].into(),
                        ..wheat_core::EditTransition::default()
                    },
                },
                wheat_core::ApplicationContext::new(&content),
            )
            .unwrap();
        wheat_store::RepositoryPublisher::new(&objects, &content, &heads)
            .initialize(&repository)
            .await
            .unwrap();

        // The revision stays retained, but an empty conflict inspection has no reason to read
        // it; only a complete repository validation walks unrelated history.
        let objects = Unreadable {
            inner: objects,
            refused: outcome.revision_id.into(),
        };
        let document = inspect_store(&objects, &content, &heads, outcome.resulting_view)
            .await
            .unwrap();
        assert!(document.conflicts.is_empty());
        let reader = RepositoryReader::new(&objects, &content);
        assert!(reader.revision(outcome.revision_id).await.is_err());
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
    fn conflict_correlation_requires_an_application_and_matching_target_view() {
        let conflict = ConflictId::from_digest([1; 32]);
        let application = (
            ChangeId::from_bytes([2; 16]),
            RevisionId::from_digest([3; 32]),
            ApplicationId::from_digest([4; 32]),
            ViewId::from_digest([5; 32]),
        );

        let missing = correlate_conflict(&mut BTreeMap::new(), conflict, application.3)
            .expect_err("missing application must be corrupt state");
        assert!(matches!(
            missing.downcast_ref::<CliFailure>(),
            Some(CliFailure::CorruptState(_))
        ));

        let mut mismatched = BTreeMap::from([(conflict, application)]);
        let mismatch = correlate_conflict(&mut mismatched, conflict, ViewId::from_digest([6; 32]))
            .expect_err("base and target views must match");
        assert!(matches!(
            mismatch.downcast_ref::<CliFailure>(),
            Some(CliFailure::CorruptState(_))
        ));
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
        assert_eq!(
            conflict["change_id"],
            serde_json::to_value(ChangeId::from_bytes([5; 16])).unwrap()
        );
        assert_eq!(
            conflict["revision_id"],
            serde_json::to_value([6_u8; 32]).unwrap()
        );
        assert_eq!(
            conflict["application_id"],
            serde_json::to_value([7_u8; 32]).unwrap()
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
                    change_id: ChangeId::from_bytes([24; 16]),
                    revision_id: RevisionId::from_digest([25; 32]),
                    application_id: ApplicationId::from_digest([26; 32]),
                    edits: vec![first_edit],
                },
                ConflictEntry {
                    id: ConflictId::from_digest([22; 32]),
                    base_view: ViewId::from_digest([23; 32]),
                    change_id: ChangeId::from_bytes([27; 16]),
                    revision_id: RevisionId::from_digest([28; 32]),
                    application_id: ApplicationId::from_digest([29; 32]),
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
