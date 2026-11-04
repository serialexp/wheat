//! Bounded-memory Git tree comparison for staged imports.
//!
//! Unlike flattening both trees, this walks only directory pairs whose object IDs differ and
//! retains only the edits which are part of the commit record.

use std::{
    collections::BTreeMap,
    io::{self, Read as _, Write as _},
    process::{Command, Stdio},
};

use gix::bstr::ByteSlice;
use wheat_core::{
    ContentId, EditId, EditRecord, EditTransition, FileKind, FileValue, RevisionEdits,
    edit_capture::{
        CaptureLimits, EditCaptureError, FileSnapshot, capture_edit_with_limits, capture_whole_file,
    },
    guarded,
    text_capture::{CaptureKind, LineEndings, StreamingCapture},
};
use wheat_store::{ContentStore, ContentStoreError, ContentWriteBatch};

use super::{MAX_IMPORT_FILE_BYTES, MAX_IMPORT_GUARDED_LINES, StagedImportError, git_error_local};
use crate::journal::ImportJournal;

const MAX_TREE_DEPTH: usize = 1024;

/// Blobs longer than this are streamed through `git cat-file` rather than read whole,
/// since gix reads an object only whole. A side this long is not guarded text, so
/// capture would record the one whole-file replacement this path records: canonical
/// text drops at most one byte per line (a CRLF's CR), so a side within
/// [`MAX_IMPORT_GUARDED_LINES`] lines is still past the guarded input bound.
const STREAMED_BLOB_BYTES: u64 =
    guarded::DEFAULT_MAX_INPUT_BYTES as u64 + MAX_IMPORT_GUARDED_LINES as u64;

/// Bytes read from `git cat-file` at a time.
const STREAM_SCRATCH_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
enum EntryKind {
    Tree,
    File(FileKind),
    Submodule,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Entry {
    oid: gix::ObjectId,
    kind: EntryKind,
}

/// Produce the first-parent guarded edits recorded by `commit`.
///
/// Captured content is put in the content store as soon as it is derived. Equal subtree IDs are
/// skipped without opening the subtree, and only the two directories at the current recursion
/// depth are materialized at once. With a `journal`, the content a long blob was stored as is
/// recorded there, so a later commit replacing it need not read it again.
pub(crate) fn tree_edits(
    git: &gix::Repository,
    commit: &gix::Commit<'_>,
    commit_oid: &[u8],
    content: &ContentStore,
    journal: Option<&ImportJournal>,
) -> Result<RevisionEdits, StagedImportError> {
    let new_tree = commit.tree_id().map_err(git_error_local)?.detach();
    let old_tree = commit
        .parent_ids()
        .next()
        .map(|id| {
            git.find_commit(id.detach())
                .map_err(git_error_local)?
                .tree_id()
                .map(|id| id.detach())
                .map_err(git_error_local)
        })
        .transpose()?;

    let mut diff = TreeDiff {
        git,
        commit_oid,
        store: content,
        journal,
        batch: ContentWriteBatch::new(content),
        edits: BTreeMap::new(),
    };
    diff.compare_directories(old_tree, Some(new_tree), "", 0)?;
    let TreeDiff {
        mut batch, edits, ..
    } = diff;
    // Never return edits while their snapshots and guard fragments are only pending.
    batch.commit()?;
    let added = edits.keys().copied().collect();
    Ok(RevisionEdits {
        edits,
        transition: EditTransition {
            added,
            ..EditTransition::default()
        },
    })
}

/// One commit's comparison against its first parent, and what it has captured so far.
struct TreeDiff<'a> {
    git: &'a gix::Repository,
    commit_oid: &'a [u8],
    store: &'a ContentStore,
    journal: Option<&'a ImportJournal>,
    batch: ContentWriteBatch<'a>,
    edits: BTreeMap<EditId, EditRecord>,
}

impl TreeDiff<'_> {
    fn compare_directories(
        &mut self,
        old: Option<gix::ObjectId>,
        new: Option<gix::ObjectId>,
        prefix: &str,
        depth: usize,
    ) -> Result<(), StagedImportError> {
        if old == new {
            return Ok(());
        }
        if depth > MAX_TREE_DEPTH {
            return Err(StagedImportError::Git(format!(
                "Git tree exceeds maximum supported depth of {MAX_TREE_DEPTH} at {prefix}"
            )));
        }

        let old_entries = directory_entries(self.git, old)?;
        let new_entries = directory_entries(self.git, new)?;
        let names = old_entries
            .keys()
            .chain(new_entries.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();

        for name in names {
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            self.compare_entries(
                old_entries.get(&name).copied(),
                new_entries.get(&name).copied(),
                &path,
                depth,
            )?;
        }
        Ok(())
    }

    fn compare_entries(
        &mut self,
        old: Option<Entry>,
        new: Option<Entry>,
        path: &str,
        directory_depth: usize,
    ) -> Result<(), StagedImportError> {
        if matches!(
            old,
            Some(Entry {
                kind: EntryKind::Submodule,
                ..
            })
        ) || matches!(
            new,
            Some(Entry {
                kind: EntryKind::Submodule,
                ..
            })
        ) {
            return Err(StagedImportError::UnsupportedSubmodule(path.to_owned()));
        }

        match (old, new) {
            (Some(old), Some(new)) if old == new => Ok(()),
            (
                Some(Entry {
                    oid: old,
                    kind: EntryKind::Tree,
                }),
                Some(Entry {
                    oid: new,
                    kind: EntryKind::Tree,
                }),
            ) => self.compare_directories(Some(old), Some(new), path, directory_depth + 1),
            (
                Some(Entry {
                    oid,
                    kind: EntryKind::Tree,
                }),
                new,
            ) => {
                self.compare_directories(Some(oid), None, path, directory_depth + 1)?;
                if let Some(Entry {
                    oid,
                    kind: EntryKind::File(kind),
                }) = new
                {
                    self.record_blob_edit(None, Some((oid, kind)), path)?;
                }
                Ok(())
            }
            (
                old,
                Some(Entry {
                    oid,
                    kind: EntryKind::Tree,
                }),
            ) => {
                if let Some(Entry {
                    oid,
                    kind: EntryKind::File(kind),
                }) = old
                {
                    self.record_blob_edit(Some((oid, kind)), None, path)?;
                }
                self.compare_directories(None, Some(oid), path, directory_depth + 1)
            }
            (old, new) => self.record_blob_edit(
                old.and_then(|entry| match entry.kind {
                    EntryKind::File(kind) => Some((entry.oid, kind)),
                    _ => None,
                }),
                new.and_then(|entry| match entry.kind {
                    EntryKind::File(kind) => Some((entry.oid, kind)),
                    _ => None,
                }),
                path,
            ),
        }
    }

    fn record_blob_edit(
        &mut self,
        old: Option<(gix::ObjectId, FileKind)>,
        new: Option<(gix::ObjectId, FileKind)>,
        path: &str,
    ) -> Result<(), StagedImportError> {
        if old == new {
            return Ok(());
        }
        let old_length = old.map(|(oid, _)| blob_length(self.git, oid)).transpose()?;
        let new_length = new.map(|(oid, _)| blob_length(self.git, oid)).transpose()?;
        if old_length
            .max(new_length)
            .is_some_and(|length| length > STREAMED_BLOB_BYTES)
        {
            return self.record_whole_file(old.zip(old_length), new.zip(new_length), path);
        }

        let before = old
            .map(|(oid, kind)| load_blob(self.git, oid).map(|bytes| (kind, bytes)))
            .transpose()?;
        let after = new
            .map(|(oid, kind)| load_blob(self.git, oid).map(|bytes| (kind, bytes)))
            .transpose()?;

        let captured = capture_edit_with_limits(
            path,
            before
                .as_ref()
                .map(|(kind, bytes)| FileSnapshot { kind: *kind, bytes }),
            after
                .as_ref()
                .map(|(kind, bytes)| FileSnapshot { kind: *kind, bytes }),
            None,
            CaptureLimits {
                max_file_bytes: MAX_IMPORT_FILE_BYTES,
                max_guarded_lines: MAX_IMPORT_GUARDED_LINES,
                guarded: guarded::Limits::default(),
            },
        )
        .map_err(|source| self.capture_error(path, source))?;
        // Import captures snapshots and guarded-edit fragments into one content set.
        // Its records are durable import evidence, not a compaction output: a source
        // history can retain any of them independently of the next file snapshot.
        // Store them as complete content so each import record is independently
        // readable across staged batches and restarts. Bounded reverse-delta chains
        // remain a repack-only physical-layout optimization.
        for captured in captured.content.into_values() {
            self.batch.put_verified(captured.borrowed())?;
        }
        self.edits.extend(captured.edits);
        Ok(())
    }

    /// Records a transition with a side past [`STREAMED_BLOB_BYTES`] as one whole-file
    /// replacement, streaming each side rather than holding it.
    fn record_whole_file(
        &mut self,
        old: Option<((gix::ObjectId, FileKind), u64)>,
        new: Option<((gix::ObjectId, FileKind), u64)>,
        path: &str,
    ) -> Result<(), StagedImportError> {
        let before = old
            .map(|((oid, kind), length)| self.streamed_value(oid, kind, length, true))
            .transpose()?;
        let after = new
            .map(|((oid, kind), length)| self.streamed_value(oid, kind, length, false))
            .transpose()?;
        if let Some(record) = capture_whole_file(path, before, after)
            .map_err(|source| self.capture_error(path, source))?
        {
            self.edits.insert(record.id(), record);
        }
        Ok(())
    }

    /// The value of the blob `oid` as a file of `kind`, stored. A `before` side, which
    /// an earlier commit already stored, is looked up in the journal first. Otherwise
    /// the blob is hashed as it streams, and streamed again into the store when the
    /// store lacks it. Text is canonicalized as it passes, as capture does; a symlink
    /// stays byte exact.
    fn streamed_value(
        &self,
        oid: gix::ObjectId,
        kind: FileKind,
        length: u64,
        before: bool,
    ) -> Result<FileValue, StagedImportError> {
        let symlink = kind == FileKind::Symlink;
        if before
            && let Some(journal) = self.journal
            && let Some(content) = journal.blob_content(oid.as_bytes(), symlink)?
            && self.store.contains(content)
        {
            return Ok(FileValue { content, kind });
        }

        let (content, text) = if symlink {
            let mut hasher = ContentId::hasher();
            stream_blob(self.git, oid, length, &mut |bytes| {
                hasher.update(bytes);
                Ok(())
            })?;
            (ContentId::from_digest(*hasher.finalize().as_bytes()), false)
        } else {
            let mut capture = StreamingCapture::new(0);
            stream_blob(self.git, oid, length, &mut |bytes| {
                capture.update(bytes);
                Ok(())
            })?;
            let result = capture.finish();
            (
                ContentId::from_digest(result.digest),
                result.kind == CaptureKind::Text,
            )
        };

        if !self.store.contains(content) {
            let mut writer = self.store.writer();
            if text {
                let mut line_endings = LineEndings::default();
                stream_blob(self.git, oid, length, &mut |bytes| {
                    let mut written = Ok(());
                    line_endings.update(bytes, |canonical| {
                        if written.is_ok() {
                            written = writer.write_all(canonical).map_err(write_error);
                        }
                    });
                    written
                })?;
                let mut written = Ok(());
                line_endings.finish(|canonical| {
                    if written.is_ok() {
                        written = writer.write_all(canonical).map_err(write_error);
                    }
                });
                written?;
            } else {
                stream_blob(self.git, oid, length, &mut |bytes| {
                    writer.write_all(bytes).map_err(write_error)
                })?;
            }
            writer.finish(Some(content))?;
        }
        if let Some(journal) = self.journal {
            journal.record_blob_content(oid.as_bytes(), symlink, content)?;
        }
        Ok(FileValue { content, kind })
    }

    fn capture_error(&self, path: &str, source: EditCaptureError) -> StagedImportError {
        StagedImportError::EditCapture {
            commit: self
                .commit_oid
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            path: path.to_owned(),
            source,
        }
    }
}

fn directory_entries(
    git: &gix::Repository,
    oid: Option<gix::ObjectId>,
) -> Result<BTreeMap<String, Entry>, StagedImportError> {
    let Some(oid) = oid else {
        return Ok(BTreeMap::new());
    };
    let tree = git.find_tree(oid).map_err(git_error_local)?;
    let mut entries = BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry.map_err(git_error_local)?;
        let name = entry
            .filename()
            .to_str()
            .map_err(|_| StagedImportError::InvalidUtf8("tree path"))?
            .to_owned();
        let mode = entry.mode();
        let kind = if mode.is_tree() {
            EntryKind::Tree
        } else if mode.is_commit() {
            EntryKind::Submodule
        } else if mode.is_link() {
            EntryKind::File(FileKind::Symlink)
        } else if mode.is_executable() {
            EntryKind::File(FileKind::Executable)
        } else {
            EntryKind::File(FileKind::Regular)
        };
        entries.insert(
            name,
            Entry {
                oid: entry.id().detach(),
                kind,
            },
        );
    }
    Ok(entries)
}

fn blob_length(git: &gix::Repository, oid: gix::ObjectId) -> Result<u64, StagedImportError> {
    Ok(git.find_header(oid).map_err(git_error_local)?.size())
}

fn load_blob(git: &gix::Repository, oid: gix::ObjectId) -> Result<Vec<u8>, StagedImportError> {
    let blob = git
        .find_object(oid)
        .map_err(git_error_local)?
        .try_into_blob()
        .map_err(git_error_local)?;
    Ok(blob.data.clone())
}

/// A [`wheat_store::ContentWriter`] failure as the store error it carries.
fn write_error(error: io::Error) -> StagedImportError {
    match error.downcast::<ContentStoreError>() {
        Ok(error) => error.into(),
        Err(error) => ContentStoreError::from(error).into(),
    }
}

/// Passes the blob `oid`, which is `length` bytes, to `out` a piece at a time, read from
/// `git cat-file`. A blob of another length is refused once that shows, and a failure
/// of `out` stops the read.
fn stream_blob(
    git: &gix::Repository,
    oid: gix::ObjectId,
    length: u64,
    out: &mut dyn FnMut(&[u8]) -> Result<(), StagedImportError>,
) -> Result<(), StagedImportError> {
    let failed =
        |message: String| StagedImportError::Git(format!("git cat-file blob {oid}: {message}"));
    let mut child = Command::new("git")
        .arg("--git-dir")
        .arg(git.git_dir())
        .args(["cat-file", "blob"])
        .arg(oid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| failed(error.to_string()))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut scratch = vec![0_u8; STREAM_SCRATCH_BYTES];
    let mut read = 0_u64;
    let streamed = loop {
        let count = match stdout.read(&mut scratch) {
            Ok(0) => break Ok(()),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => break Err(failed(error.to_string())),
        };
        read = read.saturating_add(count as u64);
        if read > length {
            break Err(failed(format!("more than its {length} bytes")));
        }
        if let Err(error) = out(&scratch[..count]) {
            break Err(error);
        }
    };
    drop(stdout);
    if streamed.is_err() {
        // The reader stopped early; don't wait on a writer blocked on a full pipe.
        let _ = child.kill();
    }
    let output = child
        .wait_with_output()
        .map_err(|error| failed(error.to_string()))?;
    streamed?;
    if !output.status.success() {
        return Err(failed(format!(
            "{}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if read != length {
        return Err(failed(format!("{read} bytes, not {length}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whole_file_capture(text: &[u8]) -> bool {
        let captured = capture_edit_with_limits(
            "file",
            Some(FileSnapshot {
                kind: FileKind::Regular,
                bytes: b"before\n",
            }),
            Some(FileSnapshot {
                kind: FileKind::Regular,
                bytes: text,
            }),
            None,
            CaptureLimits {
                max_file_bytes: MAX_IMPORT_FILE_BYTES,
                max_guarded_lines: MAX_IMPORT_GUARDED_LINES,
                guarded: guarded::Limits::default(),
            },
        )
        .unwrap();
        captured
            .edits
            .values()
            .all(|edit| matches!(edit.operation, wheat_core::EditOperation::ExactFile { .. }))
    }

    /// `length` bytes of text: one long line, then as many CRLF lines as a guarded
    /// side may have, which is what canonicalizing shrinks the most.
    fn crlf_text(length: usize) -> Vec<u8> {
        let mut text = vec![b'x'; length - 2 * MAX_IMPORT_GUARDED_LINES];
        text.extend(b"\r\n".repeat(MAX_IMPORT_GUARDED_LINES));
        text
    }

    #[test]
    fn a_streamed_side_is_one_capture_records_whole() {
        let streamed = usize::try_from(STREAMED_BLOB_BYTES).unwrap();
        assert!(whole_file_capture(&crlf_text(streamed + 1)));
        // The bound is tight: a byte shorter and the text is guarded.
        assert!(!whole_file_capture(&crlf_text(streamed)));
    }
}
