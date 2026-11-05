//! One large file through a server: added, published, cloned, edited by one byte, and
//! fetched.
//!
//! A writer adds a `file_mib` MiB file and publishes it to a server in this process,
//! and a reader clones it. The writer then flips one byte in the middle, adds and
//! publishes that, and the reader fetches it and refreshes onto it, which rewrites the
//! file in its workspace. No step may hold the
//! file: each process's peak stays far below its size. The edit is stored and sent as
//! the chunks around the byte and a new manifest, not as another copy of the file.

use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Result, ensure};
use wheat_core::{ContentId, MAX_CONTENT_CHUNK_BYTES};

use super::{
    CaseContext, CaseOutput, Recorder, content_large_blob::directory_bytes,
    publish_content::write_noise, server::Server, workspace::Workspace,
};
use crate::{measure::PhasePeak, path_text};

pub(crate) const NAME: &str = "large_file";
const MIB: usize = 1024 * 1024;
const PATH: &str = "large.bin";
/// What an edit of one byte may store or send: the chunk the byte is in, perhaps its
/// neighbour, the new manifest, and the change's metadata objects.
const EDIT_BUDGET_BYTES: f64 = 2.0 * MAX_CONTENT_CHUNK_BYTES as f64 + MIB as f64;
/// Budget for each process's peak while the file passes through it: a chunk or a
/// few, a batch, and the process itself, but never the file. At 1 GiB (2026-11-05)
/// the highest was 153 MiB, the server's during the clone.
const PEAK_BUDGET_BYTES: f64 = 192.0 * MIB as f64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let file_mib = context.scale.pick(1024, 24);
    let length = file_mib * MIB;
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("file_mib", file_mib);

    let source = Workspace::new(context)?;
    let server = Server::start(source.root())?;
    let url = server.url().to_owned();
    let writer = Client::new(context, "writer");
    let reader = Client::new(context, "reader");
    writer.clone_from(&source, &url, &context.scratch)?;

    allow_file(&writer.root, length)?;
    write_noise(&writer.root.join(PATH), 0, length)?;
    let added = writer.change(&source, "large file")?;
    recorder.timing("add", added.timing);
    recorder.value("add.bytes_stored", writer.stored()? as f64, "bytes");

    let server_before = directory_bytes(server.store())?;
    let mut peak = PhasePeak::start()?;
    let published = writer.run(&source, &["publish"])?;
    recorder.peak_rss("publish_server", peak.take()?);
    recorder.timing("publish", published.timing);
    let server_after = directory_bytes(server.store())?;
    recorder.value(
        "publish.bytes_stored",
        (server_after - server_before) as f64,
        "bytes",
    );

    let cloned = reader.clone_from(&source, &url, &context.scratch)?;
    recorder.peak_rss("clone_server", peak.take()?);
    recorder.timing("clone", cloned.timing);
    ensure!(
        file_id(&reader.root.join(PATH))? == file_id(&writer.root.join(PATH))?,
        "the clone's file differs from the published one"
    );
    let reader_before = reader.stored()?;

    // One byte in the middle changes.
    flip_byte(&writer.root.join(PATH), length as u64 / 2)?;
    let writer_before = writer.stored()?;
    let edited = writer.change(&source, "one byte")?;
    recorder.timing("edit_add", edited.timing);
    let edit_stored = writer.stored()? - writer_before;
    recorder.value("edit_add.bytes_stored", edit_stored as f64, "bytes");

    let server_before = directory_bytes(server.store())?;
    let published = writer.run(&source, &["publish"])?;
    recorder.peak_rss("edit_publish_server", peak.take()?);
    recorder.timing("edit_publish", published.timing);
    let edit_published = directory_bytes(server.store())? - server_before;
    recorder.value("edit_publish.bytes_stored", edit_published as f64, "bytes");

    let fetched = reader.run(&source, &["fetch"])?;
    recorder.peak_rss("fetch_server", peak.take()?);
    recorder.timing("fetch", fetched.timing);
    let edit_fetched = reader.stored()? - reader_before;
    recorder.value("fetch.bytes_stored", edit_fetched as f64, "bytes");
    // The fetch moved label `main` to the edit; refresh follows it and materializes.
    let refreshed = reader.run(&source, &["refresh"])?;
    recorder.timing("refresh", refreshed.timing);
    ensure!(
        file_id(&reader.root.join(PATH))? == file_id(&writer.root.join(PATH))?,
        "the fetched file differs from the published edit\nfetch: {}\nrefresh: {}",
        String::from_utf8_lossy(&fetched.stdout),
        String::from_utf8_lossy(&refreshed.stdout)
    );
    server.stop()?;

    for step in [
        "add",
        "publish",
        "publish_server",
        "clone",
        "clone_server",
        "edit_add",
        "edit_publish",
        "edit_publish_server",
        "fetch",
        "refresh",
    ] {
        recorder.at_most(&format!("{step}.peak_rss"), PEAK_BUDGET_BYTES, "bytes");
    }
    for step in ["edit_add", "edit_publish", "fetch"] {
        recorder.at_most(&format!("{step}.bytes_stored"), EDIT_BUDGET_BYTES, "bytes");
    }
    Ok(recorder.finish())
}

/// A workspace cloned from the server, with a store home of its own so the stores it
/// writes are its own.
struct Client {
    root: PathBuf,
    stores: PathBuf,
}

impl Client {
    fn new(context: &CaseContext, name: &str) -> Self {
        Self {
            root: context.scratch.join(name),
            stores: context.scratch.join(format!("{name}-stores")),
        }
    }

    fn clone_from(
        &self,
        source: &Workspace,
        url: &str,
        scratch: &Path,
    ) -> Result<crate::measure::Captured> {
        source.ok_in_stores(
            scratch,
            &self.stores,
            &["clone", url, path_text(&self.root)?],
        )
    }

    fn run(&self, source: &Workspace, arguments: &[&str]) -> Result<crate::measure::Captured> {
        source.ok_in_stores(&self.root, &self.stores, arguments)
    }

    /// Captures the workspace's `large.bin` as a new change.
    fn change(&self, source: &Workspace, message: &str) -> Result<crate::measure::Captured> {
        self.run(source, &["change", "new", "--message", message])?;
        self.run(source, &["add", PATH])
    }

    /// The bytes in the client's stores.
    fn stored(&self) -> Result<u64> {
        directory_bytes(&self.stores)
    }
}

/// Raises the workspace's file limit (`.wheat.json`) to `length`.
pub(crate) fn allow_file(root: &Path, length: usize) -> Result<()> {
    fs::write(
        root.join(".wheat.json"),
        format!(r#"{{"format":"wheat-repository","version":1,"max_file_bytes":{length}}}"#),
    )?;
    Ok(())
}

/// Flips the byte at `offset` in `path` in place.
pub(crate) fn flip_byte(path: &Path, offset: u64) -> Result<()> {
    let mut file = fs::OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut byte)?;
    byte[0] ^= 1;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(&byte)?;
    Ok(())
}

/// The content ID of the file at `path`, read a piece at a time.
pub(crate) fn file_id(path: &Path) -> Result<ContentId> {
    let mut file = fs::File::open(path)?;
    let mut hasher = ContentId::hasher();
    let mut buffer = vec![0; MIB];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(ContentId::from_digest(*hasher.finalize().as_bytes()))
}
