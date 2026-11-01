//! A large blob through a packed store's streaming content API.
//!
//! Streams a `blob_mib` MiB blob into `ContentStore::writer` in 1 MiB pieces, then
//! a copy with one byte changed in the middle, then reads the first back through
//! `ContentStore::reader`. Counts what each holds and writes: the writer holds at
//! most the chunking threshold, the reader one chunk, and the edit writes only the
//! chunks around it and a manifest.

use std::{
    io::{Read, Write},
    path::Path,
    time::Instant,
};

use anyhow::{Result, ensure};
use wheat_core::{CHUNKED_CONTENT_THRESHOLD, ContentId, MAX_CONTENT_CHUNK_BYTES};
use wheat_store::{ContentStore, PackedObjectStore};

use super::{CaseContext, CaseOutput, Recorder};

pub(crate) const NAME: &str = "content_large_blob";
const MIB: usize = 1 << 20;
const PIECE: usize = MIB;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let blob_mib = context.scale.pick(1024, 24);
    let length = blob_mib * MIB;
    let edit_at = length / 2;
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("blob_mib", blob_mib);

    let root = tempfile::tempdir_in(&context.scratch)?;
    let storage = root.path().join("storage");
    let objects = PackedObjectStore::open(&storage)?;
    let content = objects.content_store();

    let started = Instant::now();
    let (first, held) = write(&content, length, None)?;
    recorder.wall("write", started.elapsed().as_secs_f64());
    let written = directory_bytes(&storage.join("loose-content"))?;
    recorder.value("write.held_bytes", held as f64, "bytes");
    recorder.value("write.bytes_written", written as f64, "bytes");

    let started = Instant::now();
    let (second, edit_held) = write(&content, length, Some(edit_at))?;
    recorder.wall("edit", started.elapsed().as_secs_f64());
    ensure!(first != second, "the edit did not change the blob");
    let edit_written = directory_bytes(&storage.join("loose-content"))? - written;
    recorder.value("edit.held_bytes", edit_held as f64, "bytes");
    recorder.value("edit.bytes_written", edit_written as f64, "bytes");

    let started = Instant::now();
    let mut reader = content.reader(first)?;
    let mut buffer = vec![0; PIECE];
    // The reader checks the blob's ID at its end; this checks the reader's bytes.
    let mut hasher = ContentId::hasher();
    let mut read_held = 0;
    let mut read = 0;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        read_held = read_held.max(reader.held_bytes());
        hasher.update(&buffer[..count]);
        read += count;
    }
    recorder.wall("read", started.elapsed().as_secs_f64());
    ensure!(read == length, "read {read} of {length} bytes");
    ensure!(
        ContentId::from_digest(*hasher.finalize().as_bytes()) == first,
        "the blob read back differs"
    );
    recorder.value("read.held_bytes", read_held as f64, "bytes");

    let threshold = CHUNKED_CONTENT_THRESHOLD as f64;
    let chunk = f64::from(MAX_CONTENT_CHUNK_BYTES);
    recorder.at_most("write.held_bytes", threshold, "bytes");
    recorder.at_most("edit.held_bytes", threshold, "bytes");
    recorder.at_most("read.held_bytes", chunk, "bytes");
    // The chunk the edit falls in, perhaps its neighbour, and the new manifest.
    recorder.at_most(
        "edit.bytes_written",
        2.0 * chunk + (64 * 1024) as f64,
        "bytes",
    );
    Ok(recorder.finish())
}

/// Streams `length` bytes of [`Noise`] into a writer, with the byte at `edit`
/// flipped, returning the blob's ID and the most bytes the writer held.
fn write(content: &ContentStore, length: usize, edit: Option<usize>) -> Result<(ContentId, usize)> {
    let mut writer = content.writer();
    let mut noise = Noise::new(edit);
    let mut held = 0;
    let mut hasher = ContentId::hasher();
    let mut written = 0;
    while written < length {
        let piece = noise.take(PIECE.min(length - written));
        hasher.update(piece);
        writer.write_all(piece)?;
        held = held.max(writer.held_bytes());
        written += piece.len();
    }
    let expected = ContentId::from_digest(*hasher.finalize().as_bytes());
    Ok((writer.finish(Some(expected))?, held))
}

/// Xorshift bytes, which do not compress, generated a piece at a time. The stream
/// depends on the piece sizes, which are whole words but for the last.
struct Noise {
    state: u64,
    position: usize,
    edit: Option<usize>,
    piece: Vec<u8>,
}

impl Noise {
    fn new(edit: Option<usize>) -> Self {
        Self {
            state: 0x9E37_79B9_7F4A_7C15,
            position: 0,
            edit,
            piece: Vec::with_capacity(PIECE),
        }
    }

    fn take(&mut self, count: usize) -> &[u8] {
        self.piece.clear();
        while self.piece.len() < count {
            self.state ^= self.state << 13;
            self.state ^= self.state >> 7;
            self.state ^= self.state << 17;
            let take = (count - self.piece.len()).min(8);
            self.piece
                .extend_from_slice(&self.state.to_le_bytes()[..take]);
        }
        if let Some(edit) = self.edit
            && (self.position..self.position + count).contains(&edit)
        {
            self.piece[edit - self.position] ^= 1;
        }
        self.position += count;
        &self.piece
    }
}

/// The bytes of every file under `root`.
fn directory_bytes(root: &Path) -> Result<u64> {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}
