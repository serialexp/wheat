//! Bounded-memory Git tree comparison for staged imports.
//!
//! Unlike flattening both trees, this walks only directory pairs whose object IDs differ and
//! retains only the edits which are part of the commit record.

use std::collections::BTreeMap;

use gix::bstr::ByteSlice;
use wheat_core::{
    ContentId, EditId, EditRecord, EditTransition, FileKind, RevisionEdits,
    edit_capture::{CaptureLimits, FileSnapshot, capture_edit_with_limits},
    guarded,
};
use wheat_store::ContentStore;

use super::{MAX_IMPORT_FILE_BYTES, MAX_IMPORT_GUARDED_LINES, StagedImportError, git_error_local};

const MAX_TREE_DEPTH: usize = 1024;

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
/// depth are materialized at once.
pub(super) fn tree_edits(
    git: &gix::Repository,
    commit: &gix::Commit<'_>,
    commit_oid: &[u8],
    content: &ContentStore,
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

    let mut edits = BTreeMap::new();
    let mut batch = wheat_store::ContentWriteBatch::new(content);
    compare_directories(
        git,
        old_tree,
        Some(new_tree),
        "",
        0,
        commit_oid,
        &mut batch,
        &mut edits,
    )?;
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

fn compare_directories(
    git: &gix::Repository,
    old: Option<gix::ObjectId>,
    new: Option<gix::ObjectId>,
    prefix: &str,
    depth: usize,
    commit_oid: &[u8],
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<EditId, EditRecord>,
) -> Result<(), StagedImportError> {
    if old == new {
        return Ok(());
    }
    if depth > MAX_TREE_DEPTH {
        return Err(StagedImportError::Git(format!(
            "Git tree exceeds maximum supported depth of {MAX_TREE_DEPTH} at {prefix}"
        )));
    }

    let old_entries = directory_entries(git, old)?;
    let new_entries = directory_entries(git, new)?;
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
        compare_entries(
            git,
            old_entries.get(&name).copied(),
            new_entries.get(&name).copied(),
            &path,
            depth,
            commit_oid,
            content,
            edits,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compare_entries(
    git: &gix::Repository,
    old: Option<Entry>,
    new: Option<Entry>,
    path: &str,
    directory_depth: usize,
    commit_oid: &[u8],
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<EditId, EditRecord>,
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
        ) => compare_directories(
            git,
            Some(old),
            Some(new),
            path,
            directory_depth + 1,
            commit_oid,
            content,
            edits,
        ),
        (
            Some(Entry {
                oid,
                kind: EntryKind::Tree,
            }),
            new,
        ) => {
            compare_directories(
                git,
                Some(oid),
                None,
                path,
                directory_depth + 1,
                commit_oid,
                content,
                edits,
            )?;
            if let Some(Entry {
                oid,
                kind: EntryKind::File(kind),
            }) = new
            {
                record_blob_edit(
                    git,
                    None,
                    Some((oid, kind)),
                    path,
                    commit_oid,
                    content,
                    edits,
                )?;
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
                record_blob_edit(
                    git,
                    Some((oid, kind)),
                    None,
                    path,
                    commit_oid,
                    content,
                    edits,
                )?;
            }
            compare_directories(
                git,
                None,
                Some(oid),
                path,
                directory_depth + 1,
                commit_oid,
                content,
                edits,
            )
        }
        (old, new) => record_blob_edit(
            git,
            old.and_then(|entry| match entry.kind {
                EntryKind::File(kind) => Some((entry.oid, kind)),
                _ => None,
            }),
            new.and_then(|entry| match entry.kind {
                EntryKind::File(kind) => Some((entry.oid, kind)),
                _ => None,
            }),
            path,
            commit_oid,
            content,
            edits,
        ),
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

fn record_blob_edit(
    git: &gix::Repository,
    old: Option<(gix::ObjectId, FileKind)>,
    new: Option<(gix::ObjectId, FileKind)>,
    path: &str,
    commit_oid: &[u8],
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<EditId, EditRecord>,
) -> Result<(), StagedImportError> {
    if old == new {
        return Ok(());
    }
    let before = old
        .map(|(oid, kind)| load_blob(git, oid).map(|bytes| (kind, bytes)))
        .transpose()?;
    let after = new
        .map(|(oid, kind)| load_blob(git, oid).map(|bytes| (kind, bytes)))
        .transpose()?;

    // When both old and new blobs exist (a modification), compute the old
    // content's ContentId so we can delta-encode new content against it.
    let base_content_id = match (&before, &after) {
        (Some((old_kind, old_bytes)), Some(_)) => Some(canonical_content_id(old_bytes, *old_kind)),
        _ => None,
    };

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
    .map_err(|source| StagedImportError::EditCapture {
        commit: commit_oid
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        path: path.to_owned(),
        source,
    })?;
    for (id, bytes) in captured.content {
        let stored = if let Some(base_id) = base_content_id {
            // Try delta encoding against the old version of this file.
            // put_with_base falls back to raw if the delta isn't worthwhile.
            content.put_with_base(&bytes, base_id)?
        } else {
            content.put(&bytes)?
        };
        debug_assert_eq!(stored, id);
    }
    edits.extend(captured.edits);
    Ok(())
}

fn load_blob(git: &gix::Repository, oid: gix::ObjectId) -> Result<Vec<u8>, StagedImportError> {
    let blob = git
        .find_object(oid)
        .map_err(git_error_local)?
        .try_into_blob()
        .map_err(git_error_local)?;
    Ok(blob.data.clone())
}

/// Compute the `ContentId` of a Git blob after applying the same text
/// canonicalization that `capture_edit_with_limits` uses internally.
/// Symlinks are stored raw; everything else goes through text capture
/// (CR/CRLF → LF normalization, binary detection).
fn canonical_content_id(bytes: &[u8], kind: FileKind) -> ContentId {
    use std::borrow::Cow;
    use wheat_core::text_capture::{CapturedBytes, capture};

    if kind == FileKind::Symlink {
        return ContentId::of(bytes);
    }
    match capture(bytes) {
        CapturedBytes::Text(Cow::Borrowed(text)) => ContentId::of(text.as_bytes()),
        CapturedBytes::Text(Cow::Owned(text)) => ContentId::of(text.as_bytes()),
        CapturedBytes::Binary(blob) => ContentId::of(&blob),
    }
}
