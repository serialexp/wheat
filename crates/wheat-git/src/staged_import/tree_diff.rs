//! Bounded-memory Git tree comparison for staged imports.
//!
//! Unlike flattening both trees, this walks only directory pairs whose object IDs differ and
//! retains only the edits which are part of the commit record.

use std::collections::BTreeMap;

use gix::bstr::ByteSlice;
use wheat_core::{ContentId, FileEdit};
use wheat_store::ContentStore;

use super::{StagedImportError, git_error_local};

const MAX_TREE_DEPTH: usize = 1024;

#[derive(Clone, Copy)]
enum EntryKind {
    Tree,
    Blob,
    Submodule,
}

#[derive(Clone, Copy)]
struct Entry {
    oid: gix::ObjectId,
    kind: EntryKind,
}

/// Produce the first-parent transformation recorded by `commit`.
///
/// Blob contents are put in the content store as soon as they are read. Equal subtree IDs are
/// skipped without opening the subtree, and only the two directories at the current recursion
/// depth are materialized at once.
pub(super) fn tree_edits(
    git: &gix::Repository,
    commit: &gix::Commit<'_>,
    content: &ContentStore,
) -> Result<Vec<FileEdit>, StagedImportError> {
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
    compare_directories(git, old_tree, Some(new_tree), "", 0, &mut batch, &mut edits)?;
    // The commit writer reads these IDs from the durable backing store.
    // Never return edits while their content is only in the pending tail.
    batch.commit()?;
    Ok(edits.into_values().collect())
}

fn compare_directories(
    git: &gix::Repository,
    old: Option<gix::ObjectId>,
    new: Option<gix::ObjectId>,
    prefix: &str,
    depth: usize,
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<String, FileEdit>,
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
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<String, FileEdit>,
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
        (Some(old), Some(new)) if old.oid == new.oid => Ok(()),
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
                content,
                edits,
            )?;
            if let Some(Entry {
                oid,
                kind: EntryKind::Blob,
            }) = new
            {
                record_blob_edit(git, None, Some(oid), path, content, edits)?;
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
                kind: EntryKind::Blob,
            }) = old
            {
                record_blob_edit(git, Some(oid), None, path, content, edits)?;
            }
            compare_directories(
                git,
                None,
                Some(oid),
                path,
                directory_depth + 1,
                content,
                edits,
            )
        }
        (old, new) => record_blob_edit(
            git,
            old.map(|entry| entry.oid),
            new.map(|entry| entry.oid),
            path,
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
        } else {
            EntryKind::Blob
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
    old: Option<gix::ObjectId>,
    new: Option<gix::ObjectId>,
    path: &str,
    content: &mut wheat_store::ContentWriteBatch<'_>,
    edits: &mut BTreeMap<String, FileEdit>,
) -> Result<(), StagedImportError> {
    if old == new {
        return Ok(());
    }
    let expected = old.map(|oid| store_blob(git, oid, content)).transpose()?;
    let replacement = new.map(|oid| store_blob(git, oid, content)).transpose()?;
    // Git modes are not represented in a FileEdit. This also protects against mode-only changes
    // if distinct object references happen to resolve to identical bytes.
    if expected != replacement {
        let path = path.to_owned();
        edits.insert(
            path.clone(),
            FileEdit {
                path,
                expected,
                replacement,
            },
        );
    }
    Ok(())
}

fn store_blob(
    git: &gix::Repository,
    oid: gix::ObjectId,
    content: &mut wheat_store::ContentWriteBatch<'_>,
) -> Result<ContentId, StagedImportError> {
    let blob = git
        .find_object(oid)
        .map_err(git_error_local)?
        .try_into_blob()
        .map_err(git_error_local)?;
    content.put(&blob.data).map_err(Into::into)
}
