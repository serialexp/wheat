//! Durable publication catalog for immutable packed segments.
//!
//! The SQLite database is deliberately disposable: every open authenticates the
//! immutable catalog chain and all referenced segments, then builds a fresh
//! locator.  It is never a source of truth.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    SegmentCatalogRecord, SegmentRecordError, decode_segment_catalog_record,
    encode_segment_catalog_record,
};

use super::segment::{SegmentBuilder, SegmentError, SegmentReader, SegmentRecord};

const SCHEMA: &str = include_str!("catalog_sql/schema.sql");
const LOOKUP: &str = include_str!("catalog_sql/lookup.sql");
const SEGMENT_SUFFIX: &str = ".segment";
const CATALOG_SUFFIX: &str = ".catalog";

#[derive(Debug, Error)]
pub(crate) enum CatalogError {
    #[error("packed catalog is already open by another process")]
    Busy,
    #[error("catalog I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("catalog database failed: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("catalog record failed: {0}")]
    Record(#[from] SegmentRecordError),
    #[error("segment failed: {0}")]
    Segment(#[from] SegmentError),
    #[error("catalog chain is corrupt: {0}")]
    Corrupt(&'static str),
}

struct CatalogItem {
    record: SegmentCatalogRecord,
    digest: [u8; 32],
}

/// A process-exclusive writer/read facade over a packed-store root.
pub(crate) struct PackedCatalog {
    root: PathBuf,
    segments: PathBuf,
    catalogs: PathBuf,
    _lock: File,
    index: Connection,
    _index_file: NamedTempFile,
    generation: Option<u64>,
    head: Option<[u8; 32]>,
    // Segments are immutable and bounded to 1,024 index entries. Retain their
    // authenticated indexes for the catalog lifetime; evicting them caused
    // random-ID verification to repeatedly hash and decode entire segments.
    readers: HashMap<String, SegmentReader>,
}

impl PackedCatalog {
    pub(crate) fn open(root: impl AsRef<Path>) -> Result<Self, CatalogError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        let segments = root.join("segments");
        let catalogs = root.join("catalog");
        fs::create_dir_all(&segments)?;
        fs::create_dir_all(&catalogs)?;
        let mut ancestor = Some(root.as_path());
        while let Some(path) = ancestor {
            let path = if path.as_os_str().is_empty() {
                Path::new(".")
            } else {
                path
            };
            sync_directory(path)?;
            ancestor = if path == Path::new(".") {
                None
            } else {
                path.parent()
            };
        }

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("catalog.lock"))?;
        try_lock_exclusive(&lock)?;

        let index_file = NamedTempFile::new_in(&root)?;
        let index = Connection::open(index_file.path())?;
        index.execute_batch(SCHEMA)?;
        let mut catalog = Self {
            root,
            segments,
            catalogs,
            _lock: lock,
            index,
            _index_file: index_file,
            generation: None,
            head: None,
            readers: HashMap::new(),
        };
        catalog.rebuild_index()?;
        Ok(catalog)
    }

    /// Seals and atomically publishes one new segment and one catalog record.
    pub(crate) fn put_records(
        &mut self,
        records: &[SegmentRecord<'_>],
    ) -> Result<(), CatalogError> {
        if records.is_empty() {
            return Ok(());
        }
        let generation = self
            .generation
            .map_or(Some(0), |g| g.checked_add(1))
            .ok_or(CatalogError::Corrupt("generation overflow"))?;

        let mut builder = SegmentBuilder::new(&self.root)?;
        let mut added = false;
        for record in records {
            if let Some((kind, bytes)) = self.get(record.namespace, record.digest)? {
                if kind != record.kind || bytes != record.bytes {
                    return Err(CatalogError::Corrupt(
                        "existing key differs from proposed bytes",
                    ));
                }
                continue;
            }
            builder.push(*record)?;
            added = true;
        }
        if !added {
            return Ok(());
        }
        let mut sealed = builder.seal()?;
        sealed.file.as_file_mut().sync_all()?;
        let segment_name = format!("{}-{}{}", generation, hex(&sealed.id.0), SEGMENT_SUFFIX);
        let segment_path = self.segments.join(&segment_name);
        if let Err(error) = sealed.file.persist_noclobber(&segment_path) {
            if error.error.kind() != io::ErrorKind::AlreadyExists {
                return Err(CatalogError::Io(error.error));
            }
            let existing = SegmentReader::open(File::open(&segment_path)?)?;
            if existing.id().0 != sealed.id.0
                || existing.metadata().total_length != sealed.metadata.total_length
            {
                return Err(CatalogError::Corrupt("conflicting orphan segment"));
            }
        }
        sync_directory(&self.segments)?;

        let record = SegmentCatalogRecord {
            generation,
            previous: self.head,
            segment: sealed.id.0,
            stored_length: sealed.metadata.total_length,
        };
        let encoded = encode_segment_catalog_record(&record)?;
        let catalog_digest = *blake3::hash(&encoded).as_bytes();
        let catalog_name = format!("{}-{}.catalog", generation, hex(&catalog_digest));
        let mut temporary = NamedTempFile::new_in(&self.root)?;
        temporary.write_all(&encoded)?;
        temporary.as_file_mut().sync_all()?;
        let catalog_path = self.catalogs.join(catalog_name);
        if let Err(error) = temporary.persist_noclobber(&catalog_path) {
            if error.error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.error.into());
            }
            if fs::read(&catalog_path)? != encoded {
                return Err(CatalogError::Corrupt("conflicting catalog retry"));
            }
        }
        sync_directory(&self.catalogs)?;

        // Publication is complete at this point. If incremental indexing fails,
        // discard all derived state and reconstruct it from the durable chain.
        if self.index_segment(&segment_name, &segment_path).is_err() {
            self.rebuild_index()?;
            return Ok(());
        }
        self.generation = Some(generation);
        self.head = Some(catalog_digest);
        Ok(())
    }

    pub(crate) fn get(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<(u16, Vec<u8>)>, CatalogError> {
        let filename: Option<String> = self
            .index
            .query_row(LOOKUP, params![namespace, digest.as_slice()], |row| {
                row.get(0)
            })
            .optional()?;
        let Some(filename) = filename else {
            return Ok(None);
        };
        self.ensure_reader(&filename)?;
        let value = self
            .readers
            .get_mut(&filename)
            .ok_or(CatalogError::Corrupt("reader cache"))?
            .lookup(namespace, digest)?;
        if value.is_none() {
            return Err(CatalogError::Corrupt("locator disagrees with segment"));
        }
        Ok(value)
    }

    fn rebuild_index(&mut self) -> Result<(), CatalogError> {
        self.index.execute("DELETE FROM locator", [])?;
        self.readers.clear();
        let items = read_chain(&self.catalogs)?;
        for item in &items {
            let filename = format!(
                "{}-{}.segment",
                item.record.generation,
                hex(&item.record.segment)
            );
            let path = self.segments.join(&filename);
            let metadata = fs::metadata(&path).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    CatalogError::Corrupt("catalog references missing segment")
                } else {
                    CatalogError::Io(error)
                }
            })?;
            if metadata.len() != item.record.stored_length {
                return Err(CatalogError::Corrupt("stored segment length"));
            }
            self.index_segment(&filename, &path)?;
        }
        self.generation = items.last().map(|item| item.record.generation);
        self.head = items.last().map(|item| item.digest);
        Ok(())
    }

    fn index_segment(&mut self, filename: &str, path: &Path) -> Result<(), CatalogError> {
        let mut reader = SegmentReader::open(File::open(path)?)?;
        if !filename.ends_with(&format!("-{}.segment", hex(&reader.id().0))) {
            return Err(CatalogError::Corrupt(
                "segment identity disagrees with catalog",
            ));
        }
        let entries = reader.entries()?;
        let transaction = self.index.transaction()?;
        for entry in entries {
            transaction.execute(
                "INSERT INTO locator(namespace,digest,segment_file) VALUES(?1,?2,?3) \
                 ON CONFLICT(namespace,digest) DO UPDATE SET segment_file=excluded.segment_file",
                params![entry.namespace, entry.digest.as_slice(), filename],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn ensure_reader(&mut self, filename: &str) -> Result<(), CatalogError> {
        if !self.readers.contains_key(filename) {
            let reader = SegmentReader::open(File::open(self.segments.join(filename))?)?;
            self.readers.insert(filename.to_owned(), reader);
        }
        Ok(())
    }
}

fn read_chain(directory: &Path) -> Result<Vec<CatalogItem>, CatalogError> {
    let mut items = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or(CatalogError::Corrupt("catalog filename"))?;
        if !name.ends_with(CATALOG_SUFFIX) {
            continue;
        }
        let encoded = fs::read(entry.path())?;
        let record = decode_segment_catalog_record(&encoded)?;
        let digest = *blake3::hash(&encoded).as_bytes();
        let expected = format!("{}-{}{}", record.generation, hex(&digest), CATALOG_SUFFIX);
        if name != expected {
            return Err(CatalogError::Corrupt(
                "catalog filename does not match record",
            ));
        }
        items.push(CatalogItem { record, digest });
    }
    items.sort_unstable_by_key(|item| item.record.generation);
    for (position, item) in items.iter().enumerate() {
        if item.record.generation != position as u64 {
            return Err(CatalogError::Corrupt("generation gap or fork"));
        }
        let expected_previous = position.checked_sub(1).map(|i| items[i].digest);
        if item.record.previous != expected_previous {
            return Err(CatalogError::Corrupt("broken previous-record hash"));
        }
    }
    Ok(items)
}

fn try_lock_exclusive(file: &File) -> Result<(), CatalogError> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) || error.raw_os_error() == Some(libc::EAGAIN)
    {
        Err(CatalogError::Busy)
    } else {
        Err(CatalogError::Io(error))
    }
}

fn sync_directory(path: &Path) -> Result<(), io::Error> {
    File::open(path)?.sync_all()
}

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
