//! Bounded expansion and atomic rendering of durable conflicts.

use std::{
    io::{self, Write},
    num::NonZeroUsize,
    path::Path,
};

use anyhow::Result;
use serde::Serialize;
use wheat_core::{
    ApplicationId, ChangeId, EditFailure, EditId, FileKind, FileValue, RevisionId, ViewId,
};
use wheat_store::{
    BrowseError, ConflictIndex, ConflictIndexError, ConflictPageLimits, ContentStore,
    ImmutableObjectStore, PublicationStore, RepositoryBrowser,
};

use crate::{
    local::{CONFLICT_INDEX_FILE, LocalRepository},
    outcome::CliFailure,
};

pub const MAX_CONFLICTS: usize = 100_000;
pub const MAX_EDIT_OCCURRENCES: usize = 250_000;
pub const MAX_PATH_BYTES: usize = 4_096;
const MAX_AGGREGATE_PATH_BYTES: usize = 16 * 1024 * 1024;
const MAX_MODEL_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// How many conflicts one index page reads.
const PAGE_CONFLICTS: NonZeroUsize = NonZeroUsize::new(256).unwrap();

#[derive(Debug, Serialize)]
pub struct ConflictDocument {
    pub schema_version: u8,
    pub head_generation: u64,
    /// The effective view's unresolved conflicts in canonical order, earliest first.
    pub conflicts: Vec<ConflictEntry>,
}

#[derive(Debug, Serialize)]
pub struct ConflictEntry {
    pub id: wheat_core::ConflictId,
    /// The conflicted change's index in the view's canonical order.
    pub position: u64,
    pub base_view: ViewId,
    pub change_id: ChangeId,
    /// The conflicted change's title, so a person can tell which change it is.
    pub title: String,
    pub revision_id: RevisionId,
    pub application_id: ApplicationId,
    pub edits: Vec<ConflictEdit>,
}

#[derive(Debug, Serialize)]
pub struct ConflictEdit {
    pub id: EditId,
    pub path: String,
    pub target: Option<FileValue>,
    pub reason: EditFailure,
}

/// The unresolved conflicts of `effective_view` in the store at `store_root`, read
/// through the store's conflict index, which catches up first.
pub async fn inspect(
    repository: &LocalRepository,
    store_root: &Path,
    effective_view: ViewId,
) -> Result<ConflictDocument> {
    let index =
        ConflictIndex::open(&store_root.join(CONFLICT_INDEX_FILE)).map_err(index_failure)?;
    inspect_store(
        &repository.objects,
        &repository.content,
        &repository.heads,
        &index,
        effective_view,
    )
    .await
}

async fn inspect_store<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    heads: &impl PublicationStore,
    index: &ConflictIndex,
    effective_view: ViewId,
) -> Result<ConflictDocument> {
    let head = heads.read()?;
    // Every typed read authenticates the immutable object's bytes against the requested ID,
    // and the browser refuses a view the head doesn't retain. Only the roots this head
    // selects are followed: complete validation is reserved for `wheat validate`.
    let browser = RepositoryBrowser::open(objects, content, head)
        .await
        .map_err(browse_failure)?;
    let total = browser
        .conflict_count(index, effective_view)
        .await
        .map_err(browse_failure)?;
    if total > MAX_CONFLICTS as u64 {
        return Err(CliFailure::limit("conflict count exceeds inspection limit"));
    }
    let mut entries = Vec::with_capacity(total as usize);
    let mut edit_occurrences = 0usize;
    let mut aggregate_path_bytes = 0usize;
    let mut model_bytes = 0usize;
    let mut after = None;
    loop {
        let Some(edits) = NonZeroUsize::new(MAX_EDIT_OCCURRENCES - edit_occurrences) else {
            return Err(CliFailure::limit(
                "conflict edit occurrence count exceeds inspection limit",
            ));
        };
        let page = browser
            .conflicts(
                index,
                effective_view,
                after,
                ConflictPageLimits {
                    conflicts: PAGE_CONFLICTS,
                    edits,
                },
            )
            .await
            .map_err(browse_failure)?;
        for item in page.items {
            if item.edits.len() < item.edit_count {
                return Err(CliFailure::limit(
                    "conflict edit occurrence count exceeds inspection limit",
                ));
            }
            edit_occurrences += item.edits.len();
            model_bytes = bounded_add(
                model_bytes,
                64usize.saturating_add(item.title.len()),
                MAX_MODEL_BYTES,
                "conflict model",
            )?;
            let mut edits = Vec::with_capacity(item.edits.len());
            for edit in item.edits {
                if edit.path.len() > MAX_PATH_BYTES {
                    return Err(CliFailure::limit(
                        "conflict edit path exceeds inspection limit",
                    ));
                }
                aggregate_path_bytes = bounded_add(
                    aggregate_path_bytes,
                    edit.path.len(),
                    MAX_AGGREGATE_PATH_BYTES,
                    "aggregate conflict path bytes",
                )?;
                // IDs, target, enum tag and interference IDs are all retained in the model.
                let reason_bytes = match &edit.reason {
                    EditFailure::Interference { with } => with.len().saturating_mul(32),
                    _ => 0,
                };
                model_bytes = bounded_add(
                    model_bytes,
                    66usize
                        .saturating_add(edit.path.len())
                        .saturating_add(reason_bytes),
                    MAX_MODEL_BYTES,
                    "conflict model",
                )?;
                edits.push(ConflictEdit {
                    id: edit.edit,
                    path: edit.path,
                    target: edit.found,
                    reason: edit.reason,
                });
            }
            entries.push(ConflictEntry {
                id: item.conflict,
                position: item.position,
                base_view: item.target_view,
                change_id: item.change,
                title: item.title,
                revision_id: item.revision,
                application_id: item.application,
                edits,
            });
        }
        match page.next {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    Ok(ConflictDocument {
        schema_version: 1,
        head_generation: head.generation,
        conflicts: entries,
    })
}

/// A browse read's failure as the store error behind it, so the command reports it by
/// that error's kind.
fn browse_failure(error: BrowseError) -> anyhow::Error {
    match error {
        BrowseError::Read(error) => error.into(),
        BrowseError::Tree(error) => error.into(),
        BrowseError::Content(error) => error.into(),
        BrowseError::ConflictIndex(error) => index_failure(error),
        BrowseError::IdentityMismatch(what) => CliFailure::corrupt(format!(
            "repository record {what} does not match its identity"
        )),
        BrowseError::ViewNotRetained(_) => {
            CliFailure::corrupt("effective view is not retained by the current repository state")
        }
        error => error.into(),
    }
}

fn index_failure(error: ConflictIndexError) -> anyhow::Error {
    match error {
        ConflictIndexError::Read(error) => error.into(),
        error => error.into(),
    }
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

pub fn human(document: &ConflictDocument, destination: &mut impl Write) -> Result<()> {
    atomic_render(destination, MAX_OUTPUT_BYTES, |output| {
        for conflict in &document.conflicts {
            // Debug string formatting quotes and escapes newlines and control bytes.
            writeln!(
                output,
                "conflict in change {} {:?} (revision {})",
                conflict.change_id, conflict.title, conflict.revision_id
            )?;
            for edit in &conflict.edits {
                writeln!(
                    output,
                    "  {:?}: {}; {}",
                    edit.path,
                    edit.reason,
                    Found(edit.target)
                )?;
            }
        }
        Ok(())
    })
}

/// What the edit found at its path where it was applied: the evidence's
/// `target`, which is the view's value, not what the edit would write.
struct Found(Option<FileValue>);

impl std::fmt::Display for Found {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => formatter.write_str("the path does not exist there"),
            Some(FileValue { content, kind }) => {
                let kind = match kind {
                    FileKind::Regular => "a file",
                    FileKind::Executable => "an executable file",
                    FileKind::Symlink => "a symlink",
                };
                write!(formatter, "it found {kind} with content {content}")
            }
        }
    }
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
    use wheat_core::{ConflictId, ContentId, FileKind, ObjectId};
    use wheat_store::RepositoryReader;

    fn document(path: &str) -> ConflictDocument {
        ConflictDocument {
            schema_version: 1,
            head_generation: 42,
            conflicts: vec![ConflictEntry {
                id: ConflictId::from_digest([1; 32]),
                position: 0,
                base_view: ViewId::from_digest([2; 32]),
                change_id: ChangeId::from_bytes([5; 16]),
                title: "Make it executable".to_owned(),
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
        let index = ConflictIndex::open(&root.path().join(CONFLICT_INDEX_FILE)).unwrap();
        let document = inspect_store(&objects, &content, &heads, &index, outcome.resulting_view)
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
    fn human_paths_are_quoted_and_control_characters_are_escaped() {
        let mut output = Vec::new();
        human(&document("line\n\tname"), &mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("  \"line\\n\\tname\": "));
        assert!(!rendered.contains("line\n"));
    }

    #[test]
    fn human_output_names_the_change_and_says_what_failed_in_words() {
        let mut output = Vec::new();
        human(&document("bin/run"), &mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        let content = ContentId::from_digest([4; 32]);
        assert_eq!(
            rendered,
            format!(
                "conflict in change {} \"Make it executable\" (revision {})\n  \"bin/run\": the file kind differs from the edit's; it found an executable file with content {content}\n",
                ChangeId::from_bytes([5; 16]),
                RevisionId::from_digest([6; 32]),
            )
        );
        assert!(!rendered.contains("Some("), "no debug output: {rendered}");
    }

    #[test]
    fn json_contains_identifiers_target_and_no_fabricated_ranges() {
        let mut output = Vec::new();
        json(&document("path"), &mut output).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["head_generation"], 42);
        let conflict = &value["conflicts"][0];
        // IDs are their hex, as the human output shows them.
        let hex = |byte: u8| format!("{byte:02x}").repeat(32);
        assert_eq!(conflict["id"], hex(1));
        assert_eq!(conflict["base_view"], hex(2));
        assert_eq!(
            conflict["change_id"],
            ChangeId::from_bytes([5; 16]).to_string()
        );
        assert_eq!(conflict["title"], "Make it executable");
        assert_eq!(conflict["revision_id"], hex(6));
        assert_eq!(conflict["application_id"], hex(7));
        let edit = &conflict["edits"][0];
        assert_eq!(edit["id"], hex(3));
        assert_eq!(edit["path"], "path");
        assert_eq!(edit["target"]["content"], hex(4));
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
                    position: 1,
                    base_view: ViewId::from_digest([21; 32]),
                    change_id: ChangeId::from_bytes([24; 16]),
                    title: "first".to_owned(),
                    revision_id: RevisionId::from_digest([25; 32]),
                    application_id: ApplicationId::from_digest([26; 32]),
                    edits: vec![first_edit],
                },
                ConflictEntry {
                    id: ConflictId::from_digest([22; 32]),
                    position: 2,
                    base_view: ViewId::from_digest([23; 32]),
                    change_id: ChangeId::from_bytes([27; 16]),
                    title: "second".to_owned(),
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
