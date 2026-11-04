//! Building and walking chunk manifests. A blob of up to
//! [`MAX_DIRECT_CONTENT_CHUNKS`] chunks lists them in its manifest; a longer one
//! groups them into parts, each stored as content of its own, and lists those.
//!
//! Where a part ends is content-defined, like a chunk: after a chunk whose ID's
//! first eight bytes, as a little-endian number, are a multiple of [`PART_DIVISOR`],
//! once the part holds [`MIN_CONTENT_PART_CHUNKS`], or at
//! [`MAX_CONTENT_PART_CHUNKS`]. An edit changes the chunks around it and so the parts
//! around it; the other parts stay as they were. Every rule here is part of the
//! format: changing one changes how much new versions share with old ones, though
//! never what any `ContentId` names.

use wheat_core::{
    ContentChunkManifestRecord, ContentChunkPartRecord, ContentChunkRecord, ContentId,
    ContentManifestEntries, ContentPartRecord, MAX_CONTENT_CHUNK_PART_BYTES,
    MAX_CONTENT_PART_CHUNKS, MAX_DIRECT_CONTENT_CHUNKS, MIN_CONTENT_PART_CHUNKS, VerifiedContent,
    decode_content_chunk_part, encode_content_chunk_part,
};

use crate::{ContentStore, ContentStoreError};

/// Past the fewest chunks, a part ends after one chunk in this many on average, so
/// parts hold about 4,096 chunks.
const PART_DIVISOR: u64 = 3072;

/// How many chunk IDs [`ContentStore::confirm`] looks up at once.
const CONFIRM_BATCH: usize = 1024;

/// Whether a part holding `count` chunks, the last of them `last`, ends there.
fn part_ends(count: usize, last: ContentId) -> bool {
    if count >= MAX_CONTENT_PART_CHUNKS {
        return true;
    }
    let bytes = last.as_bytes();
    let head = u64::from_le_bytes(bytes[..8].try_into().expect("eight bytes"));
    count >= MIN_CONTENT_PART_CHUNKS && head % PART_DIVISOR == 0
}

/// Gathers a long blob's chunks, in order, into its manifest. It holds the chunks
/// of the part being gathered, and while the blob has no more chunks than a
/// manifest lists itself, every chunk so far; once it has more, it stores each part
/// as it ends.
#[derive(Debug, Default)]
pub(crate) struct ManifestBuilder {
    /// The parts ended so far, while the blob may still be listed directly.
    held: Vec<ContentChunkRecord>,
    open: Vec<ContentChunkRecord>,
    open_length: u64,
    /// The chunks given so far.
    count: usize,
    /// The parts stored so far, once the blob has too many chunks to list.
    parts: Vec<ContentPartRecord>,
}

impl ManifestBuilder {
    /// Adds the blob's next chunk. `store` stores a part, once the blob has more
    /// chunks than a manifest lists.
    pub(crate) fn push(
        &mut self,
        chunk: ContentChunkRecord,
        mut store: impl FnMut(VerifiedContent<&[u8]>) -> Result<(), ContentStoreError>,
    ) -> Result<(), ContentStoreError> {
        self.open.push(chunk);
        self.open_length += u64::from(chunk.length);
        self.count += 1;
        if self.count == MAX_DIRECT_CONTENT_CHUNKS + 1 {
            // Too many to list: the parts ended so far are stored now.
            let mut held = std::mem::take(&mut self.held).into_iter();
            let mut run = Vec::new();
            let mut length = 0;
            for chunk in held.by_ref() {
                run.push(chunk);
                length += u64::from(chunk.length);
                if part_ends(run.len(), chunk.id) {
                    self.parts.push(store_part(&run, length, &mut store)?);
                    run.clear();
                    length = 0;
                }
            }
            debug_assert!(run.is_empty(), "held chunks end a part");
        }
        if part_ends(self.open.len(), chunk.id) {
            if self.count <= MAX_DIRECT_CONTENT_CHUNKS {
                self.held.append(&mut self.open);
            } else {
                self.parts
                    .push(store_part(&self.open, self.open_length, &mut store)?);
                self.open.clear();
            }
            self.open_length = 0;
        }
        Ok(())
    }

    /// The manifest of the blob of `logical_length` bytes whose chunks were given,
    /// storing its last part first when it has parts.
    pub(crate) fn finish(
        mut self,
        logical_length: u64,
        mut store: impl FnMut(VerifiedContent<&[u8]>) -> Result<(), ContentStoreError>,
    ) -> Result<ContentChunkManifestRecord, ContentStoreError> {
        let entries = if self.count <= MAX_DIRECT_CONTENT_CHUNKS {
            self.held.append(&mut self.open);
            ContentManifestEntries::Chunks(self.held)
        } else {
            if !self.open.is_empty() {
                self.parts
                    .push(store_part(&self.open, self.open_length, &mut store)?);
            }
            ContentManifestEntries::Parts(self.parts)
        };
        Ok(ContentChunkManifestRecord {
            logical_length,
            entries,
        })
    }
}

fn store_part(
    chunks: &[ContentChunkRecord],
    length: u64,
    store: &mut impl FnMut(VerifiedContent<&[u8]>) -> Result<(), ContentStoreError>,
) -> Result<ContentPartRecord, ContentStoreError> {
    let bytes = encode_content_chunk_part(&ContentChunkPartRecord {
        chunks: chunks.to_vec(),
    })
    .map_err(|error| ContentStoreError::Io(std::io::Error::other(error)))?;
    let content = VerifiedContent::new(bytes.as_slice());
    store(content)?;
    Ok(ContentPartRecord {
        id: content.id(),
        length,
    })
}

/// Walks a manifest's chunks in order, reading each part as it reaches it, so it
/// holds one part's chunk list at a time.
#[derive(Debug)]
pub(crate) struct ManifestChunks {
    /// The blob, named in errors.
    id: ContentId,
    chunks: std::vec::IntoIter<ContentChunkRecord>,
    parts: std::vec::IntoIter<ContentPartRecord>,
}

impl ManifestChunks {
    pub(crate) fn new(id: ContentId, manifest: ContentChunkManifestRecord) -> Self {
        let (chunks, parts) = match manifest.entries {
            ContentManifestEntries::Chunks(chunks) => (chunks, Vec::new()),
            ContentManifestEntries::Parts(parts) => (Vec::new(), parts),
        };
        Self {
            id,
            chunks: chunks.into_iter(),
            parts: parts.into_iter(),
        }
    }

    /// The next chunk. `load` reads the part named, bounded by
    /// [`MAX_CONTENT_CHUNK_PART_BYTES`], when the walk reaches it.
    pub(crate) fn next(
        &mut self,
        load: impl FnOnce(ContentId, usize) -> Result<Vec<u8>, ContentStoreError>,
    ) -> Result<Option<ContentChunkRecord>, ContentStoreError> {
        if let Some(chunk) = self.chunks.next() {
            return Ok(Some(chunk));
        }
        let Some(part) = self.parts.next() else {
            return Ok(None);
        };
        let bytes = load(part.id, MAX_CONTENT_CHUNK_PART_BYTES)?;
        let invalid = |error: wheat_core::StoreRecordError| ContentStoreError::InvalidFormat {
            id: self.id,
            reason: format!("part {}: {error}", part.id),
        };
        let record = decode_content_chunk_part(&bytes).map_err(invalid)?;
        record
            .validate(part.length, self.parts.len() == 0)
            .map_err(invalid)?;
        self.chunks = record.chunks.into_iter();
        Ok(self.chunks.next())
    }
}

impl ContentStore {
    /// The manifest of `id` when it is stored as chunks; `None` for a blob stored
    /// whole.
    pub fn chunk_manifest(
        &self,
        id: ContentId,
    ) -> Result<Option<ContentChunkManifestRecord>, ContentStoreError> {
        if !crate::content_store::is_chunked_length(self.logical_length(id)?) {
            return Ok(None);
        }
        match self.stored(id)? {
            crate::content_store::StoredContent::Envelope(header, payload)
                if header.codec == crate::codec::Codec::Chunked =>
            {
                crate::content_stream::chunk_manifest(id, header, payload).map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Checks that the chunks `manifest` names, all stored here, hold the bytes of
    /// `id`: reads them in order through a hasher, a chunk at a time.
    pub fn check_manifest(
        &self,
        id: ContentId,
        manifest: &ContentChunkManifestRecord,
    ) -> Result<(), ContentStoreError> {
        let mut hasher = ContentId::hasher();
        let mut chunks = ManifestChunks::new(id, manifest.clone());
        while let Some(chunk) = chunks.next(|part, max| self.get_bounded(part, max))? {
            let bytes = self.get_bounded(chunk.id, chunk.length as usize)?;
            if bytes.len() != chunk.length as usize {
                return Err(ContentStoreError::InvalidFormat {
                    id,
                    reason: format!("chunk {} is not as long as its manifest says", chunk.id),
                });
            }
            hasher.update(&bytes);
        }
        let actual = ContentId::from_digest(*hasher.finalize().as_bytes());
        if actual != id {
            return Err(ContentStoreError::Corrupt {
                expected: id,
                actual,
            });
        }
        Ok(())
    }
    /// Reads `id` and checks its bytes against its ID without returning them. A
    /// chunked blob is read a chunk at a time through [`Self::check_manifest`], so
    /// at most one chunk is held; a blob stored whole is read whole.
    pub fn verify(&self, id: ContentId) -> Result<(), ContentStoreError> {
        match self.chunk_manifest(id)? {
            Some(manifest) => self.check_manifest(id, &manifest),
            None => self.get_verified(id).map(drop),
        }
    }

    /// Confirms `id` is stored and readable without returning its bytes. A blob
    /// stored whole is read and checked against its ID. A chunked blob's manifest
    /// and parts are read and every chunk they name looked up, without reading the
    /// chunks: each was checked against its ID when it was stored, and the manifest
    /// against the blob's.
    pub fn confirm(&self, id: ContentId) -> Result<(), ContentStoreError> {
        let Some(manifest) = self.chunk_manifest(id)? else {
            return self.get_verified(id).map(drop);
        };
        let mut chunks = ManifestChunks::new(id, manifest);
        let mut batch = Vec::with_capacity(CONFIRM_BATCH);
        while let Some(chunk) = chunks.next(|part, max| self.get_bounded(part, max))? {
            batch.push(chunk.id);
            if batch.len() == CONFIRM_BATCH {
                self.confirm_present(&batch)?;
                batch.clear();
            }
        }
        self.confirm_present(&batch)
    }

    fn confirm_present(&self, ids: &[ContentId]) -> Result<(), ContentStoreError> {
        let present = self.existing_ids(ids)?;
        match ids.iter().find(|id| !present.contains(id)) {
            Some(&missing) => Err(ContentStoreError::Missing(missing)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u32 = 1 << 20;

    /// Chunks of one MiB with IDs from a counter: none ends a part early unless
    /// its ID says so.
    fn chunk(n: u64) -> ContentChunkRecord {
        ContentChunkRecord {
            id: ContentId::of(&n.to_le_bytes()),
            length: MIB,
        }
    }

    fn build(count: u64) -> (ContentChunkManifestRecord, Vec<Vec<u8>>) {
        let mut stored = Vec::new();
        let mut store = |content: VerifiedContent<&[u8]>| {
            stored.push(content.bytes().to_vec());
            Ok(())
        };
        let mut builder = ManifestBuilder::default();
        for n in 0..count {
            builder.push(chunk(n), &mut store).unwrap();
        }
        let manifest = builder.finish(count * u64::from(MIB), &mut store).unwrap();
        (manifest, stored)
    }

    #[test]
    fn a_blob_of_up_to_the_direct_limit_lists_its_chunks_and_stores_no_parts() {
        let (manifest, stored) = build(MAX_DIRECT_CONTENT_CHUNKS as u64);
        manifest.validate().unwrap();
        let ContentManifestEntries::Chunks(chunks) = &manifest.entries else {
            panic!("expected chunks");
        };
        assert_eq!(chunks.len(), MAX_DIRECT_CONTENT_CHUNKS);
        assert!(stored.is_empty());
    }

    #[test]
    fn a_longer_blob_lists_parts_that_hold_its_chunks_in_order() {
        let count = 40_000;
        let (manifest, stored) = build(count);
        manifest.validate().unwrap();
        let ContentManifestEntries::Parts(parts) = &manifest.entries else {
            panic!("expected parts");
        };
        assert_eq!(parts.len(), stored.len());
        let by_id: std::collections::HashMap<ContentId, &Vec<u8>> = stored
            .iter()
            .map(|bytes| (ContentId::of(bytes), bytes))
            .collect();
        let mut walk = ManifestChunks::new(ContentId::of(b"blob"), manifest.clone());
        let mut n = 0;
        while let Some(next) = walk
            .next(|id, max| {
                let bytes = by_id[&id].to_vec();
                assert!(bytes.len() <= max);
                Ok(bytes)
            })
            .unwrap()
        {
            assert_eq!(next, chunk(n));
            n += 1;
        }
        assert_eq!(n, count);
        // Parts end where the rule says: none but the last below the fewest chunks.
        let sizes: Vec<u64> = parts
            .iter()
            .map(|part| part.length / u64::from(MIB))
            .collect();
        let (last, rest) = sizes.split_last().unwrap();
        assert!(*last >= 1);
        for &size in rest {
            assert!(
                (MIN_CONTENT_PART_CHUNKS as u64..=MAX_CONTENT_PART_CHUNKS as u64).contains(&size)
            );
        }
    }

    #[test]
    fn part_boundaries_are_pinned() {
        let (manifest, _) = build(40_000);
        let ContentManifestEntries::Parts(parts) = manifest.entries else {
            panic!("expected parts");
        };
        let sizes: Vec<u64> = parts
            .iter()
            .map(|part| part.length / u64::from(MIB))
            .collect();
        assert_eq!(
            sizes,
            [14_212, 5_990, 2_249, 1_640, 2_902, 1_301, 3_978, 7_728]
        );
    }

    #[test]
    fn an_insertion_changes_only_the_parts_around_it() {
        let ids = |manifest: &ContentChunkManifestRecord| match &manifest.entries {
            ContentManifestEntries::Parts(parts) => {
                parts.iter().map(|part| part.id).collect::<Vec<_>>()
            }
            ContentManifestEntries::Chunks(_) => panic!("expected parts"),
        };
        let (before, _) = build(40_000);
        let mut store = |_: VerifiedContent<&[u8]>| Ok(());
        let mut builder = ManifestBuilder::default();
        for n in 0..40_000 {
            if n == 20_000 {
                builder.push(chunk(1_000_000), &mut store).unwrap();
            }
            builder.push(chunk(n), &mut store).unwrap();
        }
        let after = builder.finish(40_001 * u64::from(MIB), &mut store).unwrap();
        let (before, after) = (ids(&before), ids(&after));
        let shared = after.iter().filter(|id| before.contains(id)).count();
        assert!(
            shared + 2 >= before.len(),
            "{shared} of {} parts shared",
            before.len()
        );
    }
}
