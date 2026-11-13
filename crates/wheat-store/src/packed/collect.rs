//! Pool garbage collection's rewrite of a packed catalog: the sealed segments are
//! replaced by segments holding only the records a caller keeps.
//!
//! A collection has three steps, so the expensive one runs without the catalog's
//! lock and reads go on beside it:
//!
//! 1. [`PackedCatalog::begin_collection`], under the lock: seal the active segment
//!    and capture the sealed segments and the catalog's authority.
//! 2. [`build_collection`], without the lock: read every record of the captured
//!    segments through readers of its own, one segment at a time, and write the
//!    kept ones into new segment files, still temporary.
//! 3. [`PackedCatalog::publish_collection`], under the lock: check nothing was
//!    written meanwhile, publish the new segments through the same `repack.intent`
//!    a repack uses, and hand the replaced segments back as [`RetiredSegments`].
//!
//! The replaced segments are not removed when the new catalog is published: whoever
//! holds the [`RetiredSegments`] can still read them, and they are removed when it is
//! dropped. A crash before then leaves files no catalog names, which the next open
//! removes.

use super::*;
use crate::packed::segment::CONTENT_NAMESPACE;

/// The segments a collection rewrites, as the catalog held them when it began.
pub(crate) struct CollectionSources {
    authority: CatalogPhysicalAuthority,
    generation_before: u64,
    root: PathBuf,
    segments: PathBuf,
    sources: Vec<SealedSource>,
}

/// What a collection kept and dropped, by namespace.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CollectionCounts {
    pub objects_kept: u64,
    pub object_bytes_kept: u64,
    pub objects_dropped: u64,
    pub object_bytes_dropped: u64,
    pub content_kept: u64,
    pub content_dropped: u64,
    pub content_bytes_dropped: u64,
    /// The replacement segments, and the bytes they take.
    pub segments_written: u64,
    pub bytes_written: u64,
}

/// Replacement segments a collection wrote, not yet published.
pub(crate) struct BuiltCollection {
    sources: CollectionSources,
    replacements: Vec<SealedSegment>,
    pub(crate) counts: CollectionCounts,
}

/// Segments a published collection replaced: readable until this is dropped, then
/// removed.
pub(crate) struct RetiredSegments {
    readers: Vec<SegmentReader>,
    paths: Vec<PathBuf>,
    pub(crate) segments: usize,
    pub(crate) bytes: u64,
}

impl RetiredSegments {
    /// The record `namespace`/`digest` in a replaced segment, verified.
    pub(crate) fn get(
        &mut self,
        namespace: u8,
        digest: [u8; 32],
    ) -> Result<Option<VerifiedPayload>, CatalogError> {
        for reader in &mut self.readers {
            if let Some((_, payload)) =
                reader.lookup_entry_bounded(namespace, digest, usize::MAX)?
            {
                return Ok(Some(payload));
            }
        }
        Ok(None)
    }
}

impl Drop for RetiredSegments {
    fn drop(&mut self) {
        // Readers first: a file is removed once nothing has it open. A file that cannot
        // be removed now is not named by the catalog, so the next open removes it.
        self.readers.clear();
        for path in &self.paths {
            let _ = fs::remove_file(path);
        }
    }
}

impl PackedCatalog {
    /// Seals the active segment and captures the sealed segments a collection
    /// rewrites. Nothing may be written to the store until the collection is
    /// published: [`Self::publish_collection`] refuses a catalog that changed.
    pub(crate) fn begin_collection(&mut self) -> Result<CollectionSources, CatalogError> {
        if self.durability != Durability::Durable || self.bulk.is_some() || self.poisoned {
            return Err(CatalogError::RepackUnsupported);
        }
        self.prepare_write()?;
        self.seal_active()?;
        self.load_sealed_readers()?;
        Ok(CollectionSources {
            authority: self.physical_authority()?,
            generation_before: self.generation.unwrap_or(0),
            root: self.root.clone(),
            segments: self.segments.clone(),
            sources: self.sealed_sources.clone(),
        })
    }

    /// Publishes `built` as the catalog's sealed segments, with `content` as its
    /// content generation, and returns the segments it replaced. Fails, with nothing
    /// published, if the catalog changed since [`Self::begin_collection`].
    pub(crate) fn publish_collection(
        &mut self,
        built: BuiltCollection,
        content: PhysicalContentGeneration,
    ) -> Result<RetiredSegments, CatalogError> {
        let BuiltCollection {
            sources,
            replacements,
            counts: _,
        } = built;
        if self.physical_authority()? != sources.authority {
            return Err(CatalogError::RepackStaleAuthority);
        }
        let next_generation = sources
            .generation_before
            .checked_add(1)
            .ok_or(CatalogError::Corrupt("generation overflow"))?;

        self.poisoned = true;
        let mut descriptors = Vec::with_capacity(replacements.len());
        let mut new_sources = Vec::with_capacity(replacements.len());
        let mut new_readers = HashMap::with_capacity(replacements.len());
        for (sequence, replacement) in replacements.into_iter().enumerate() {
            let sequence = u64::try_from(sequence)
                .map_err(|_| CatalogError::Corrupt("segment sequence overflow"))?;
            let name = format!(
                "{next_generation}-{sequence}-{}{}",
                hex(&replacement.id.0),
                SEGMENT_SUFFIX
            );
            let path = self.segments.join(&name);
            replacement
                .file
                .persist_noclobber(&path)
                .map_err(|error| error.error)?;
            let file = File::open(&path)?;
            self.durability.sync_file(&file)?;
            let reader = SegmentReader::open(file)?;
            if reader.id() != replacement.id {
                return Err(CatalogError::Corrupt("replacement segment identity"));
            }
            descriptors.push(PhysicalSegmentDescriptor {
                generation: next_generation,
                sequence,
                segment: replacement.id.0,
                stored_length: replacement.metadata.total_length,
            });
            new_sources.push(SealedSource {
                path: format!("segments/{name}"),
                generation: next_generation,
                sequence,
                segment: replacement.id.0,
                stored_length: replacement.metadata.total_length,
            });
            new_readers.insert(replacement.id.0, reader);
        }
        self.durability.sync_directory(&self.segments)?;
        let snapshot = PhysicalCatalogSnapshot {
            generation: next_generation,
            segments: descriptors,
            content,
        };
        let digest =
            publish_physical_snapshot(&self.root, &self.catalogs, &snapshot, self.durability)?;
        write_repack_intent(
            &self.root,
            self.generation.zip(self.head),
            (next_generation, digest),
            self.durability,
        )?;
        write_catalog_head(
            &self.root,
            &self.root.join(CATALOG_HEAD_FILE),
            Some((next_generation, digest)),
            self.durability,
        )?;

        self.generation = Some(next_generation);
        self.head = Some(digest);
        self.content_generation = content;
        let mut old_readers = std::mem::replace(&mut self.sealed_readers, new_readers);
        self.sealed_bytes = stored_length_sum(&new_sources)?;
        let old_sources = std::mem::replace(&mut self.sealed_sources, new_sources);
        self.rebuild_index()?;
        remove_unreachable_catalogs(
            &self.catalogs,
            self.generation.zip(self.head),
            self.durability,
        )?;
        remove_durable_file(
            &self.root,
            &self.root.join(REPACK_INTENT_FILE),
            self.durability,
        )?;
        self.poisoned = false;

        let mut retired = RetiredSegments {
            readers: Vec::with_capacity(old_sources.len()),
            paths: Vec::with_capacity(old_sources.len()),
            segments: old_sources.len(),
            bytes: stored_length_sum(&old_sources)?,
        };
        for source in old_sources {
            // A replacement can have the same contents, and so the same ID, as a
            // segment it replaced; the new catalog reads that one under its new name.
            if !self.sealed_readers.contains_key(&source.segment)
                && let Some(reader) = old_readers.remove(&source.segment)
            {
                retired.readers.push(reader);
            }
            retired.paths.push(self.root.join(source.path));
        }
        Ok(retired)
    }
}

/// What a collection does with one record of a segment it rewrites.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Keep {
    /// Copy it into the replacement segments.
    Copy,
    /// Leave it out: nothing keeps it.
    Drop,
    /// Leave it out: an earlier segment's copy of it was kept.
    Duplicate,
}

/// Writes the records of `sources` that `keep` keeps into new segments of about
/// `target_segment_bytes` each. Reads through readers of its own, so it needs no
/// lock on the catalog.
///
/// `keep` sees every record of every segment once, in segment order, and is asked
/// again for a record a later segment holds too; it answers [`Keep::Duplicate`] for
/// one it already kept. Memory is one segment's index and one record body at a time;
/// each kept record is read once.
pub(crate) fn build_collection(
    sources: CollectionSources,
    keep: &mut dyn FnMut(&ListedRecord) -> Result<Keep, CatalogError>,
    target_segment_bytes: u64,
) -> Result<BuiltCollection, CatalogError> {
    let target = target_segment_bytes.clamp(1, MAX_SEGMENT_PAYLOAD_BYTES);
    let mut counts = CollectionCounts::default();
    let mut replacements = Vec::new();
    let mut builder = SegmentBuilder::new(&sources.segments)?;
    let mut builder_is_empty = true;
    for source in &sources.sources {
        let file = File::open(sources.root.join(&source.path))?;
        if file.metadata()?.len() != source.stored_length {
            return Err(CatalogError::Corrupt("authoritative source length"));
        }
        let mut reader = SegmentReader::open(file)?;
        if reader.id().0 != source.segment {
            return Err(CatalogError::Corrupt(
                "segment identity disagrees with catalog",
            ));
        }
        let records = reader
            .authenticated_entries()
            .iter()
            .map(listed)
            .collect::<Vec<_>>();
        for record in records {
            let content = record.namespace == CONTENT_NAMESPACE;
            match keep(&record)? {
                Keep::Duplicate => continue,
                Keep::Drop => {
                    if content {
                        counts.content_dropped += 1;
                        counts.content_bytes_dropped += record.logical_length;
                    } else {
                        counts.objects_dropped += 1;
                        counts.object_bytes_dropped += record.logical_length;
                    }
                    continue;
                }
                Keep::Copy => {}
            }
            if content {
                counts.content_kept += 1;
            } else {
                counts.objects_kept += 1;
                counts.object_bytes_kept += record.logical_length;
            }
            let (entry, body) = reader
                .lookup_entry_bounded(record.namespace, record.digest, usize::MAX)?
                .ok_or(CatalogError::Corrupt("authenticated segment entry missing"))?;
            if entry.kind != record.kind || entry.logical_length != record.logical_length {
                return Err(CatalogError::Corrupt(
                    "segment entry changed while collected",
                ));
            }
            // Only the record being written is kept in memory. A delta record is read
            // resolved, so its copy is whole and needs no base.
            reader.clear_block_cache();
            let whole = prepare_record(body.record())?;
            if !builder_is_empty
                && builder
                    .projected_payload_bytes()
                    .checked_add(whole.stored_length())
                    .is_none_or(|bytes| bytes > target)
            {
                replacements.push(builder.seal()?);
                builder = SegmentBuilder::new(&sources.segments)?;
            }
            builder.push_prepared(whole)?;
            builder_is_empty = false;
        }
    }
    if !builder_is_empty {
        replacements.push(builder.seal()?);
    }
    counts.segments_written = replacements.len() as u64;
    counts.bytes_written = replacements
        .iter()
        .map(|replacement| replacement.metadata.total_length)
        .sum();
    Ok(BuiltCollection {
        sources,
        replacements,
        counts,
    })
}
