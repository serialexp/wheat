//! Durable publication catalog for immutable packed segments.
//!
//! SQLite is persistent but remains disposable derived state. A checkpoint ties
//! it to the immutable catalog head; a missing, corrupt, or stale locator is
//! reconstructed from the authoritative catalog chain and segments.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use tempfile::NamedTempFile;
use thiserror::Error;
use wheat_core::{
    SegmentCatalogRecord, SegmentRecordError, decode_segment_catalog_record,
    encode_segment_catalog_record,
};

use super::segment::{SegmentBuilder, SegmentError, SegmentReader, SegmentRecord};
use crate::Durability;

const SCHEMA: &str = include_str!("catalog_sql/schema.sql");
const LOOKUP: &str = include_str!("catalog_sql/lookup.sql");
const SEGMENT_SUFFIX: &str = ".segment";
const CATALOG_SUFFIX: &str = ".catalog";
const HEAD_FILE: &str = "catalog.head";
const HEAD_MAGIC: &[u8; 9] = b"WHEATPACK";
const HEAD_VERSION: u8 = 1;
const HEAD_LEN: usize = HEAD_MAGIC.len() + 1 + 8 + 32;

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
    generation: Option<u64>,
    head: Option<[u8; 32]>,
    durability: Durability,
    // Segments are immutable and bounded to 1,024 index entries. Retain their
    // authenticated indexes for the catalog lifetime; evicting them caused
    // random-ID verification to repeatedly hash and decode entire segments.
    readers: HashMap<String, SegmentReader>,
}

impl PackedCatalog {
    pub(crate) fn open(
        root: impl AsRef<Path>,
        durability: Durability,
    ) -> Result<Self, CatalogError> {
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
            durability.sync_directory(path)?;
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

        let index_path = root.join("catalog-locator.sqlite3");
        let index_was_missing = !index_path.exists();
        let mut index_recreated = false;
        let index = open_index(&index_path, durability).or_else(|_| {
            remove_index_files(&index_path)?;
            index_recreated = true;
            open_index(&index_path, durability)
        })?;
        if index_was_missing || index_recreated {
            durability.sync_directory(&root)?;
        }
        let mut catalog = Self {
            root,
            segments,
            catalogs,
            _lock: lock,
            index,
            generation: None,
            head: None,
            durability,
            readers: HashMap::new(),
        };

        let head_path = catalog.root.join(HEAD_FILE);
        let durable_head = match read_catalog_head(&head_path) {
            Ok(head) => head,
            Err(CatalogError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                // One-time migration from the original directory-as-authority format. Once the
                // head exists, ordinary opens never enumerate the catalog directory.
                let head = latest_catalog_head(&catalog.catalogs)?;
                write_catalog_head(&catalog.root, &head_path, head, durability)?;
                head
            }
            Err(error) => return Err(error),
        };
        validate_catalog_head(&catalog.catalogs, durable_head)?;
        match catalog.read_checkpoint() {
            Ok(checkpoint) if checkpoint == durable_head => {
                catalog.generation = checkpoint.map(|(generation, _)| generation);
                catalog.head = checkpoint.map(|(_, head)| head);
            }
            _ => catalog.rebuild_index()?,
        }
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
        self.durability.sync_file(sealed.file.as_file_mut())?;
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
        self.durability.sync_directory(&self.segments)?;

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
        self.durability.sync_file(temporary.as_file_mut())?;
        let catalog_path = self.catalogs.join(catalog_name);
        if let Err(error) = temporary.persist_noclobber(&catalog_path) {
            if error.error.kind() != io::ErrorKind::AlreadyExists {
                return Err(error.error.into());
            }
            if fs::read(&catalog_path)? != encoded {
                return Err(CatalogError::Corrupt("conflicting catalog retry"));
            }
        }
        self.durability.sync_directory(&self.catalogs)?;
        write_catalog_head(
            &self.root,
            &self.root.join(HEAD_FILE),
            Some((generation, catalog_digest)),
            self.durability,
        )?;

        // Head publication is complete at this point. Locator rows and their
        // checkpoint become visible in one SQLite transaction. A crash before
        // that commit leaves a stale checkpoint which the next open rebuilds.
        if self
            .index_segment_and_checkpoint(
                &segment_name,
                &segment_path,
                Some((generation, catalog_digest)),
            )
            .is_err()
        {
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
        let filename: Option<String> = {
            let mut lookup = self.index.prepare_cached(LOOKUP)?;
            lookup
                .query_row(params![namespace, digest.as_slice()], |row| row.get(0))
                .optional()?
        };
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
        self.readers.clear();
        let items = read_chain(&self.catalogs)?;
        let checkpoint = items
            .last()
            .map(|item| (item.record.generation, item.digest));
        let transaction = self.index.transaction()?;
        transaction.execute("DELETE FROM locator", [])?;
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
            let (_, _, entries) = open_segment_for_index(&filename, &path)?;
            insert_entries(&transaction, &filename, &entries)?;
        }
        write_checkpoint(&transaction, checkpoint)?;
        transaction.commit()?;

        self.generation = checkpoint.map(|(generation, _)| generation);
        self.head = checkpoint.map(|(_, head)| head);
        Ok(())
    }

    fn index_segment_and_checkpoint(
        &mut self,
        filename: &str,
        path: &Path,
        checkpoint: Option<(u64, [u8; 32])>,
    ) -> Result<(), CatalogError> {
        let (filename, reader, entries) = open_segment_for_index(filename, path)?;
        let transaction = self.index.transaction()?;
        insert_entries(&transaction, &filename, &entries)?;
        write_checkpoint(&transaction, checkpoint)?;
        transaction.commit()?;
        self.readers.insert(filename, reader);
        Ok(())
    }

    fn read_checkpoint(&self) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
        let row: (Option<i64>, Option<Vec<u8>>) = self.index.query_row(
            "SELECT generation, head_digest FROM checkpoint WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        match row {
            (None, None) => Ok(None),
            (Some(generation), Some(head)) => {
                let generation = u64::try_from(generation)
                    .map_err(|_| CatalogError::Corrupt("locator checkpoint generation"))?;
                let head = head
                    .try_into()
                    .map_err(|_| CatalogError::Corrupt("locator checkpoint digest"))?;
                Ok(Some((generation, head)))
            }
            _ => Err(CatalogError::Corrupt("locator checkpoint shape")),
        }
    }

    fn ensure_reader(&mut self, filename: &str) -> Result<(), CatalogError> {
        if !self.readers.contains_key(filename) {
            let reader = SegmentReader::open(File::open(self.segments.join(filename))?)?;
            self.readers.insert(filename.to_owned(), reader);
        }
        Ok(())
    }
}

fn open_index(path: &Path, durability: Durability) -> Result<Connection, CatalogError> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.pragma_update(
        None,
        "synchronous",
        match durability {
            Durability::Durable => "FULL",
            Durability::Deferred => "OFF",
        },
    )?;
    connection.pragma_update(None, "temp_store", "MEMORY")?;
    connection.pragma_update(None, "cache_size", -8192)?;
    connection.execute_batch(SCHEMA)?;
    // Detect malformed database pages before trusting the checkpoint. This reads
    // the small derived index, never the immutable segment payloads.
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(CatalogError::Corrupt("locator database integrity"));
    }
    Ok(connection)
}

fn remove_index_files(path: &Path) -> Result<(), CatalogError> {
    let sidecar = |suffix: &str| {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        PathBuf::from(name)
    };
    for candidate in [
        path.to_path_buf(),
        sidecar("-journal"),
        sidecar("-wal"),
        sidecar("-shm"),
    ] {
        match fs::remove_file(candidate) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn read_catalog_head(path: &Path) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
    let encoded = fs::read(path)?;
    if encoded.len() == HEAD_MAGIC.len() + 1 && encoded.starts_with(HEAD_MAGIC) {
        return (encoded[HEAD_MAGIC.len()] == HEAD_VERSION)
            .then_some(None)
            .ok_or(CatalogError::Corrupt("catalog head version"));
    }
    if encoded.len() != HEAD_LEN
        || !encoded.starts_with(HEAD_MAGIC)
        || encoded[HEAD_MAGIC.len()] != HEAD_VERSION
    {
        return Err(CatalogError::Corrupt("catalog head format"));
    }
    let generation = u64::from_be_bytes(
        encoded[HEAD_MAGIC.len() + 1..HEAD_MAGIC.len() + 9]
            .try_into()
            .expect("fixed catalog head generation"),
    );
    let digest = encoded[HEAD_MAGIC.len() + 9..]
        .try_into()
        .expect("fixed catalog head digest");
    Ok(Some((generation, digest)))
}

fn write_catalog_head(
    root: &Path,
    path: &Path,
    head: Option<(u64, [u8; 32])>,
    durability: Durability,
) -> Result<(), CatalogError> {
    let mut encoded = Vec::with_capacity(HEAD_LEN);
    encoded.extend_from_slice(HEAD_MAGIC);
    encoded.push(HEAD_VERSION);
    if let Some((generation, digest)) = head {
        encoded.extend_from_slice(&generation.to_be_bytes());
        encoded.extend_from_slice(&digest);
    }
    let mut temporary = NamedTempFile::new_in(root)?;
    temporary.write_all(&encoded)?;
    durability.sync_file(temporary.as_file())?;
    temporary.persist(path).map_err(|error| error.error)?;
    durability.sync_directory(root)?;
    Ok(())
}

fn validate_catalog_head(
    directory: &Path,
    head: Option<(u64, [u8; 32])>,
) -> Result<(), CatalogError> {
    let Some((generation, digest)) = head else {
        return Ok(());
    };
    let path = directory.join(format!("{}-{}{}", generation, hex(&digest), CATALOG_SUFFIX));
    let encoded = fs::read(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CatalogError::Corrupt("catalog head references missing record")
        } else {
            CatalogError::Io(error)
        }
    })?;
    let record = decode_segment_catalog_record(&encoded)?;
    if record.generation != generation || blake3::hash(&encoded).as_bytes() != &digest {
        return Err(CatalogError::Corrupt("catalog head disagrees with record"));
    }
    Ok(())
}

fn latest_catalog_head(directory: &Path) -> Result<Option<(u64, [u8; 32])>, CatalogError> {
    let mut latest: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(CatalogError::Corrupt("catalog filename"));
        };
        let Some((generation, _)) = name.split_once('-') else {
            continue;
        };
        if !name.ends_with(CATALOG_SUFFIX) {
            continue;
        }
        let generation = generation
            .parse::<u64>()
            .map_err(|_| CatalogError::Corrupt("catalog filename generation"))?;
        if latest.as_ref().is_none_or(|(old, _)| generation > *old) {
            latest = Some((generation, entry.path()));
        }
    }
    let Some((generation, path)) = latest else {
        return Ok(None);
    };
    let encoded = fs::read(&path)?;
    let record = decode_segment_catalog_record(&encoded)?;
    let digest = *blake3::hash(&encoded).as_bytes();
    let expected = format!("{}-{}{}", record.generation, hex(&digest), CATALOG_SUFFIX);
    if generation != record.generation
        || path.file_name().and_then(|name| name.to_str()) != Some(&expected)
    {
        return Err(CatalogError::Corrupt(
            "catalog filename does not match record",
        ));
    }
    Ok(Some((generation, digest)))
}

fn open_segment_for_index(
    filename: &str,
    path: &Path,
) -> Result<(String, SegmentReader, Vec<wheat_core::SegmentIndexEntry>), CatalogError> {
    let mut reader = SegmentReader::open(File::open(path)?)?;
    if !filename.ends_with(&format!("-{}.segment", hex(&reader.id().0))) {
        return Err(CatalogError::Corrupt(
            "segment identity disagrees with catalog",
        ));
    }
    let entries = reader.entries()?;
    Ok((filename.to_owned(), reader, entries))
}

fn insert_entries(
    transaction: &Transaction<'_>,
    filename: &str,
    entries: &[wheat_core::SegmentIndexEntry],
) -> Result<(), CatalogError> {
    let mut insert = transaction.prepare_cached(
        "INSERT INTO locator(namespace,digest,segment_file) VALUES(?1,?2,?3) \
         ON CONFLICT(namespace,digest) DO UPDATE SET segment_file=excluded.segment_file",
    )?;
    for entry in entries {
        insert.execute(params![entry.namespace, entry.digest.as_slice(), filename])?;
    }
    Ok(())
}

fn write_checkpoint(
    transaction: &Transaction<'_>,
    checkpoint: Option<(u64, [u8; 32])>,
) -> Result<(), CatalogError> {
    let (generation, head) = match checkpoint {
        Some((generation, head)) => (
            Some(
                i64::try_from(generation)
                    .map_err(|_| CatalogError::Corrupt("generation overflow"))?,
            ),
            Some(head),
        ),
        None => (None, None),
    };
    transaction.execute(
        "UPDATE checkpoint SET generation = ?1, head_digest = ?2 WHERE singleton = 1",
        params![generation, head.as_ref().map(|digest| digest.as_slice())],
    )?;
    Ok(())
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

fn hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(64);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 15) as usize] as char);
    }
    result
}
