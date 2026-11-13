//! Which of a packed store's files hold it, for copying a store nothing writes to
//! (see [`crate::RepositoryDirectory::files`]).

use std::path::{Component, Path};

use super::catalog::{
    BULK_FILE_PREFIX, CATALOG_LOCK_FILE, CATALOGS_DIRECTORY, LOCATOR_FILE, NEXT_ACTIVE_FILE,
    SEGMENTS_DIRECTORY, SESSION_FILE, is_temporary_file_name,
};
use super::store::RETIRED_DIRECTORY;

const PACKED_DIRECTORY: &str = "packed";
const LOOSE_OBJECTS_DIRECTORY: &str = "loose-objects";
const LOOSE_CONTENT_DIRECTORY: &str = "loose-content";
const CONTENT_GENERATIONS_DIRECTORY: &str = "content-generations";
/// The content generation the store last read whole, and its temporary file.
pub(super) const CONTENT_GENERATION_CHECK_FILE: &str = "checked";
pub(super) const CONTENT_GENERATION_CHECK_PARTIAL: &str = "checked.partial";
/// The suffix of a loose write's temporary file (`.{id}.{pid}.tmp`).
const LOOSE_TEMPORARY_SUFFIX: &str = ".tmp";

/// What a copy of a packed store does with one of its files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoreFile {
    /// Written once and never changed: a sealed segment, a catalog snapshot, a loose
    /// object or blob, a content generation's file.
    Immutable,
    /// Replaced or appended to as the store is written (the catalog head, the active
    /// segment), and anything this module does not know, which is copied to be safe.
    Mutable,
    /// Not the store's state: its lock, the marker of an open session, the locator index (rebuilt when the copy is
    /// opened), the record of its last content check (its file times do not carry
    /// over), and the temporary and unpublished files opening a store removes, among
    /// them what a sweep removed that pinned readers can still read.
    Skipped,
}

/// The file at `relative`, a path under the store's directory.
pub(crate) fn classify(relative: &Path) -> StoreFile {
    let Some(names) = relative
        .components()
        .map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
    else {
        return StoreFile::Mutable;
    };
    match names.as_slice() {
        [RETIRED_DIRECTORY, ..] => StoreFile::Skipped,
        [PACKED_DIRECTORY, name] if is_packed_leftover(name) => StoreFile::Skipped,
        [PACKED_DIRECTORY, SEGMENTS_DIRECTORY, name]
            if is_temporary_file_name(name) || name.starts_with(BULK_FILE_PREFIX) =>
        {
            StoreFile::Skipped
        }
        [PACKED_DIRECTORY, SEGMENTS_DIRECTORY | CATALOGS_DIRECTORY, _] => StoreFile::Immutable,
        [LOOSE_OBJECTS_DIRECTORY | LOOSE_CONTENT_DIRECTORY, .., name]
            if name.ends_with(LOOSE_TEMPORARY_SUFFIX) =>
        {
            StoreFile::Skipped
        }
        [
            CONTENT_GENERATIONS_DIRECTORY,
            CONTENT_GENERATION_CHECK_FILE | CONTENT_GENERATION_CHECK_PARTIAL,
        ] => StoreFile::Skipped,
        [
            LOOSE_OBJECTS_DIRECTORY | LOOSE_CONTENT_DIRECTORY | CONTENT_GENERATIONS_DIRECTORY,
            ..,
        ] => StoreFile::Immutable,
        _ => StoreFile::Mutable,
    }
}

/// Whether `name`, in the catalog's directory, is something other than its state.
fn is_packed_leftover(name: &str) -> bool {
    name == CATALOG_LOCK_FILE
        || name == SESSION_FILE
        || name == NEXT_ACTIVE_FILE
        || name.starts_with(LOCATOR_FILE)
        || is_temporary_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_classified_by_where_they_are() {
        for (path, expected) in [
            ("packed/catalog.head", StoreFile::Mutable),
            ("packed/active.segment", StoreFile::Mutable),
            ("packed/repack.intent", StoreFile::Mutable),
            ("packed/segments/3-0-ab.segment", StoreFile::Immutable),
            ("packed/catalog/3-ab.catalog", StoreFile::Immutable),
            ("loose-objects/ab/cdef", StoreFile::Immutable),
            ("loose-content/ab/cdef", StoreFile::Immutable),
            ("content-generations/2/pack", StoreFile::Immutable),
            ("content-generations/checked", StoreFile::Skipped),
            ("content-generations/checked.partial", StoreFile::Skipped),
            ("packed/catalog.lock", StoreFile::Skipped),
            ("packed/catalog.session", StoreFile::Skipped),
            ("packed/catalog-locator.sqlite3", StoreFile::Skipped),
            ("packed/catalog-locator.sqlite3-wal", StoreFile::Skipped),
            ("packed/active.segment.next", StoreFile::Skipped),
            ("packed/.tmpAb12Cd", StoreFile::Skipped),
            ("packed/segments/.tmpAb12Cd", StoreFile::Skipped),
            ("packed/segments/bulk-4.segment", StoreFile::Skipped),
            ("loose-content/ab/.cdef.42.tmp", StoreFile::Skipped),
            ("retired/sweep-Ab12Cd/ab/cdef", StoreFile::Skipped),
            ("something-new", StoreFile::Mutable),
        ] {
            assert_eq!(classify(Path::new(path)), expected, "{path}");
        }
    }
}
