//! Streaming content: [`ContentWriter`] stores a blob of any size from bytes fed in
//! pieces, and [`ContentReader`] reads one back a chunk at a time. A blob over
//! [`CHUNKED_CONTENT_THRESHOLD`] is stored as content-defined chunks and a manifest
//! (see `docs/design/wheat-large-content.md`), so neither ever holds it whole.

use std::io;

use wheat_core::{
    CHUNKED_CONTENT_THRESHOLD, ContentChunkManifestRecord, ContentChunkRecord, ContentId,
    MAX_CHUNKED_CONTENT_BYTES, MAX_CONTENT_CHUNK_BYTES, VerifiedContent,
};

use crate::{
    ContentChunker, ContentStore, ContentStoreError, codec,
    content_manifest::{ManifestBuilder, ManifestChunks},
    content_store::{StoredContent, codec_error, is_chunked_length, resolve_delta_chain},
};

/// Stores a blob fed through [`io::Write`]. Up to the threshold it buffers the
/// bytes; past it, it cuts chunks as they come and stores each one at once, so it
/// holds at most one chunk. A chunk already stored is not written again.
///
/// Nothing names the blob until [`ContentWriter::finish`]; a writer dropped
/// unfinished leaves only chunks, which are content-addressed and harmless.
#[derive(Debug)]
pub struct ContentWriter {
    store: ContentStore,
    state: WriterState,
    /// A store error ends the writer: later writes and `finish` report it.
    failed: bool,
}

#[derive(Debug)]
enum WriterState {
    Buffering(Vec<u8>),
    /// Boxed: a hasher is about 2 KiB.
    Chunking(Box<Chunking>),
}

#[derive(Debug)]
struct Chunking {
    chunker: ContentChunker,
    hasher: blake3::Hasher,
    length: u64,
    manifest: ManifestBuilder,
}

impl Chunking {
    fn update(&mut self, store: &ContentStore, bytes: &[u8]) -> Result<(), ContentStoreError> {
        self.length += bytes.len() as u64;
        if self.length > MAX_CHUNKED_CONTENT_BYTES {
            return Err(ContentStoreError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "content is longer than the {MAX_CHUNKED_CONTENT_BYTES} bytes the format takes"
                ),
            )));
        }
        self.hasher.update(bytes);
        let manifest = &mut self.manifest;
        self.chunker
            .update(bytes, |chunk| store_chunk(store, manifest, chunk))
    }
}

/// Stores `content` unless it is stored already.
fn store_absent(
    store: &ContentStore,
    content: VerifiedContent<&[u8]>,
) -> Result<(), ContentStoreError> {
    if !store.contains(content.id()) {
        store.put_whole(content)?;
    }
    Ok(())
}

/// Stores `chunk` unless it is stored already, and adds it to the manifest.
fn store_chunk(
    store: &ContentStore,
    manifest: &mut ManifestBuilder,
    chunk: &[u8],
) -> Result<(), ContentStoreError> {
    let content = VerifiedContent::new(chunk);
    store_absent(store, content)?;
    manifest.push(
        ContentChunkRecord {
            id: content.id(),
            length: chunk.len() as u32,
        },
        |part| store_absent(store, part),
    )
}

impl ContentWriter {
    fn new(store: ContentStore) -> Self {
        Self {
            store,
            state: WriterState::Buffering(Vec::new()),
            failed: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Result<(), ContentStoreError> {
        if self.failed {
            return Err(failed_writer());
        }
        let result = self.feed_unchecked(bytes);
        self.failed = result.is_err();
        result
    }

    fn feed_unchecked(&mut self, bytes: &[u8]) -> Result<(), ContentStoreError> {
        match &mut self.state {
            WriterState::Buffering(buffer) => {
                let needed = buffer.len() + bytes.len();
                if needed as u64 <= CHUNKED_CONTENT_THRESHOLD {
                    // Grown by doubling as usual, but never past the threshold.
                    if buffer.capacity() < needed {
                        let target = (buffer.capacity() * 2)
                            .max(needed)
                            .min(CHUNKED_CONTENT_THRESHOLD as usize);
                        buffer.reserve_exact(target - buffer.len());
                    }
                    buffer.extend_from_slice(bytes);
                    return Ok(());
                }
                let buffered = std::mem::take(buffer);
                let mut chunking = Box::new(Chunking {
                    chunker: ContentChunker::new(),
                    hasher: ContentId::hasher(),
                    length: 0,
                    manifest: ManifestBuilder::default(),
                });
                chunking.update(&self.store, &buffered)?;
                drop(buffered);
                chunking.update(&self.store, bytes)?;
                self.state = WriterState::Chunking(chunking);
                Ok(())
            }
            WriterState::Chunking(chunking) => chunking.update(&self.store, bytes),
        }
    }

    /// The blob bytes the writer holds: a short blob's buffer, or the chunk it is
    /// cutting.
    #[doc(hidden)]
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        match &self.state {
            WriterState::Buffering(buffer) => buffer.capacity(),
            WriterState::Chunking(chunking) => chunking.chunker.held_bytes(),
        }
    }

    /// Stores the blob and returns its ID. With `expected`, a blob whose bytes do not
    /// hash to it is refused and nothing names it.
    pub fn finish(self, expected: Option<ContentId>) -> Result<ContentId, ContentStoreError> {
        if self.failed {
            return Err(failed_writer());
        }
        match self.state {
            WriterState::Buffering(buffer) => {
                let content = VerifiedContent::new(buffer.as_slice());
                check_expected(expected, content.id())?;
                self.store.put_whole(content)?;
                Ok(content.id())
            }
            WriterState::Chunking(chunking) => {
                let Chunking {
                    chunker,
                    hasher,
                    length,
                    mut manifest,
                } = *chunking;
                chunker.finish(|chunk| store_chunk(&self.store, &mut manifest, chunk))?;
                let id = ContentId::from_digest(*hasher.finalize().as_bytes());
                check_expected(expected, id)?;
                if !self.store.contains(id) {
                    let manifest =
                        manifest.finish(length, |part| store_absent(&self.store, part))?;
                    let envelope = codec::encode_chunked_envelope(&manifest)
                        .map_err(|error| codec_error(id, usize::MAX, error))?;
                    self.store.put_raw_envelope(id, envelope)?;
                }
                Ok(id)
            }
        }
    }
}

fn check_expected(expected: Option<ContentId>, actual: ContentId) -> Result<(), ContentStoreError> {
    match expected {
        Some(expected) if expected != actual => {
            Err(ContentStoreError::Corrupt { expected, actual })
        }
        _ => Ok(()),
    }
}

fn failed_writer() -> ContentStoreError {
    ContentStoreError::Io(io::Error::other(
        "content writer failed earlier; nothing names the blob",
    ))
}

impl io::Write for ContentWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.feed(bytes).map_err(io::Error::other)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads a blob's bytes. A whole envelope is read and verified when the reader
/// opens; a chunked blob is read a chunk at a time, each chunk verified against its
/// ID as it is read and the whole blob's ID checked when the reader reaches the end.
/// A reader that stops early has checked what it returned chunk by chunk.
#[derive(Debug)]
pub struct ContentReader {
    id: ContentId,
    length: u64,
    source: ReaderSource,
}

#[derive(Debug)]
enum ReaderSource {
    Whole {
        bytes: Vec<u8>,
        position: usize,
    },
    Chunked {
        store: ContentStore,
        chunks: ManifestChunks,
        current: Vec<u8>,
        position: usize,
        /// Boxed: a hasher is about 2 KiB.
        hasher: Box<blake3::Hasher>,
        verified: bool,
    },
}

impl ContentReader {
    fn open(store: &ContentStore, id: ContentId) -> Result<Self, ContentStoreError> {
        let stored = store.stored(id)?;
        let length = stored.logical_length();
        let stored = match stored {
            StoredContent::Envelope(header, payload) if header.codec == codec::Codec::Chunked => {
                let manifest = chunk_manifest(id, header, payload)?;
                return Ok(Self {
                    id,
                    length,
                    source: ReaderSource::Chunked {
                        store: store.clone(),
                        chunks: ManifestChunks::new(id, manifest),
                        current: Vec::new(),
                        position: 0,
                        hasher: Box::new(ContentId::hasher()),
                        verified: false,
                    },
                });
            }
            other => other,
        };
        // Read once: the first fetch is the record already in hand.
        let first = std::cell::Cell::new(Some(stored));
        let content = resolve_delta_chain(id, usize::MAX, |current| match first.take() {
            Some(stored) if current == id => Ok(stored),
            _ => store.stored(current),
        })?;
        Ok(Self {
            id,
            length,
            source: ReaderSource::Whole {
                bytes: content.into_inner(),
                position: 0,
            },
        })
    }

    #[must_use]
    pub fn id(&self) -> ContentId {
        self.id
    }

    /// The blob's length in bytes, from its stored metadata.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.length
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The most blob bytes a reader of a `length`-byte blob holds at once, known
    /// before opening it: a blob stored whole is held whole, a chunked one a chunk at
    /// a time.
    #[must_use]
    pub fn held_bound(length: u64) -> u64 {
        if is_chunked_length(length) {
            u64::from(MAX_CONTENT_CHUNK_BYTES)
        } else {
            length
        }
    }

    /// The blob bytes the reader holds: a whole blob, or the chunk it is reading.
    #[doc(hidden)]
    #[must_use]
    pub fn held_bytes(&self) -> usize {
        match &self.source {
            ReaderSource::Whole { bytes, .. } => bytes.capacity(),
            ReaderSource::Chunked { current, .. } => current.capacity(),
        }
    }
}

impl io::Read for ContentReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        match &mut self.source {
            ReaderSource::Whole { bytes, position } => {
                let rest = &bytes[*position..];
                let take = rest.len().min(out.len());
                out[..take].copy_from_slice(&rest[..take]);
                *position += take;
                Ok(take)
            }
            ReaderSource::Chunked {
                store,
                chunks,
                current,
                position,
                hasher,
                verified,
            } => {
                if out.is_empty() {
                    return Ok(0);
                }
                while *position == current.len() {
                    let next = chunks
                        .next(|part, max| store.get_bounded(part, max))
                        .map_err(io::Error::other)?;
                    let Some(chunk) = next else {
                        if !*verified {
                            let actual = ContentId::from_digest(*hasher.finalize().as_bytes());
                            if actual != self.id {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    ContentStoreError::Corrupt {
                                        expected: self.id,
                                        actual,
                                    },
                                ));
                            }
                            *verified = true;
                        }
                        return Ok(0);
                    };
                    let bytes = store
                        .get_bounded(chunk.id, chunk.length as usize)
                        .map_err(io::Error::other)?;
                    if bytes.len() != chunk.length as usize {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            ContentStoreError::InvalidFormat {
                                id: self.id,
                                reason: format!(
                                    "chunk {} is not as long as its manifest says",
                                    chunk.id
                                ),
                            },
                        ));
                    }
                    hasher.update(&bytes);
                    *current = bytes;
                    *position = 0;
                }
                let rest = &current[*position..];
                let take = rest.len().min(out.len());
                out[..take].copy_from_slice(&rest[..take]);
                *position += take;
                Ok(take)
            }
        }
    }
}

/// The `Chunked` envelope of `content`, a blob over the threshold held whole,
/// passing each of its chunks to `store_chunk` and each of its parts, when it has
/// them, to `store_part` first.
pub(crate) fn chunked_envelope<'c>(
    content: VerifiedContent<&'c [u8]>,
    mut store_chunk: impl FnMut(VerifiedContent<&'c [u8]>) -> Result<(), ContentStoreError>,
    mut store_part: impl FnMut(VerifiedContent<&[u8]>) -> Result<(), ContentStoreError>,
) -> Result<Vec<u8>, ContentStoreError> {
    let mut manifest = ManifestBuilder::default();
    for chunk in crate::chunker::chunks_of(content.into_inner()) {
        let chunk = VerifiedContent::new(chunk);
        store_chunk(chunk)?;
        let record = ContentChunkRecord {
            id: chunk.id(),
            length: chunk.len() as u32,
        };
        manifest.push(record, &mut store_part)?;
    }
    let manifest = manifest.finish(content.len() as u64, &mut store_part)?;
    codec::encode_chunked_envelope(&manifest)
        .map_err(|error| codec_error(content.id(), usize::MAX, error))
}

/// The manifest a `Chunked` envelope stores.
pub(crate) fn chunk_manifest(
    id: ContentId,
    header: codec::EnvelopeHeader,
    payload: Vec<u8>,
) -> Result<ContentChunkManifestRecord, ContentStoreError> {
    match codec::decode_payload(header.codec, payload, header.logical_length, usize::MAX)
        .map_err(|error| codec_error(id, usize::MAX, error))?
    {
        codec::DecodedPayload::Chunked(manifest) => Ok(manifest),
        _ => Err(ContentStoreError::InvalidFormat {
            id,
            reason: "content is not chunked".into(),
        }),
    }
}

impl ContentStore {
    /// A writer that stores a blob of any size; see [`ContentWriter`].
    #[must_use]
    pub fn writer(&self) -> ContentWriter {
        ContentWriter::new(self.clone())
    }

    /// A reader over `id`'s bytes; see [`ContentReader`].
    pub fn reader(&self, id: ContentId) -> Result<ContentReader, ContentStoreError> {
        ContentReader::open(self, id)
    }

    /// Copies `id` into `target` through a reader and a writer, so neither store's
    /// side holds more than a chunk of it, and returns its length. The bytes are
    /// checked against `id` as they are read and again as `target` names them.
    pub fn copy_to(&self, id: ContentId, target: &ContentStore) -> Result<u64, ContentStoreError> {
        let mut reader = self.reader(id)?;
        let mut writer = target.writer();
        let mut scratch = vec![0; COPY_SCRATCH_BYTES];
        let mut length = 0_u64;
        loop {
            let read = match io::Read::read(&mut reader, &mut scratch) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(store_error(error)),
            };
            io::Write::write_all(&mut writer, &scratch[..read]).map_err(store_error)?;
            length += read as u64;
        }
        writer.finish(Some(id))?;
        Ok(length)
    }
}

/// Bytes [`ContentStore::copy_to`] moves at a time.
const COPY_SCRATCH_BYTES: usize = 256 * 1024;

/// The store error a reader's or writer's I/O error carries.
fn store_error(error: io::Error) -> ContentStoreError {
    match error.downcast::<ContentStoreError>() {
        Ok(error) => error,
        Err(error) => ContentStoreError::Io(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use wheat_core::{
        ContentChunkPartRecord, ContentManifestEntries, ContentPartRecord,
        encode_content_chunk_part,
    };

    use super::*;

    /// A blob over the threshold stored with a manifest of parts, as a sender with
    /// another cut may send one: a single part holding all its chunks.
    #[test]
    fn a_blob_listed_in_parts_reads_back_through_reader_and_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = ContentStore::new(dir.path());
        let blob: Vec<u8> = (0..CHUNKED_CONTENT_THRESHOLD as usize + 5_000_000)
            .map(|n| (n % 251) as u8 ^ (n >> 20) as u8)
            .collect();
        let mut chunks = Vec::new();
        for chunk in crate::chunker::chunks_of(&blob) {
            let content = VerifiedContent::new(chunk);
            store.put_whole(content).unwrap();
            chunks.push(ContentChunkRecord {
                id: content.id(),
                length: chunk.len() as u32,
            });
        }
        let part = encode_content_chunk_part(&ContentChunkPartRecord { chunks }).unwrap();
        let part_id = store.put(&part).unwrap();
        let id = ContentId::of(&blob);
        let manifest = ContentChunkManifestRecord {
            logical_length: blob.len() as u64,
            entries: ContentManifestEntries::Parts(vec![ContentPartRecord {
                id: part_id,
                length: blob.len() as u64,
            }]),
        };
        store
            .put_raw_envelope(id, codec::encode_chunked_envelope(&manifest).unwrap())
            .unwrap();

        let mut read = Vec::new();
        store.reader(id).unwrap().read_to_end(&mut read).unwrap();
        assert!(read == blob, "the reader returns the blob");
        assert!(store.get(id).unwrap() == blob, "get assembles the blob");
    }

    /// A part that names a length its chunks do not hold is refused, not read.
    #[test]
    fn a_part_that_does_not_hold_its_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = ContentStore::new(dir.path());
        let blob = vec![7; CHUNKED_CONTENT_THRESHOLD as usize + 1];
        let mut chunks = Vec::new();
        for chunk in crate::chunker::chunks_of(&blob) {
            chunks.push(ContentChunkRecord {
                id: store.put(chunk).unwrap(),
                length: chunk.len() as u32,
            });
        }
        // Drop the last chunk from the part, so it holds less than it says.
        chunks.pop();
        let part = encode_content_chunk_part(&ContentChunkPartRecord { chunks }).unwrap();
        let manifest = ContentChunkManifestRecord {
            logical_length: blob.len() as u64,
            entries: ContentManifestEntries::Parts(vec![ContentPartRecord {
                id: store.put(&part).unwrap(),
                length: blob.len() as u64,
            }]),
        };
        let id = ContentId::of(&blob);
        store
            .put_raw_envelope(id, codec::encode_chunked_envelope(&manifest).unwrap())
            .unwrap();
        let error = store.get(id).unwrap_err();
        assert!(
            matches!(error, ContentStoreError::InvalidFormat { .. }),
            "{error:?}"
        );
        let mut read = Vec::new();
        assert!(store.reader(id).unwrap().read_to_end(&mut read).is_err());
    }
}
