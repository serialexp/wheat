//! Publishing and cloning many megabytes through the native protocol.
//!
//! The server once kept every uploaded blob in memory until the upload ended, so its
//! peak grew with the size of the publish, and the client could not publish a blob
//! over one 16 MiB message. Uploads are now stored in batches of at most 64 MiB as
//! they arrive, and blobs over 4 MiB travel in chunks both ways. A clone publishes
//! `files` blobs of `file_mib` MiB each (every one chunked) to a server running in this
//! process, then a fresh clone downloads them. The server's peak during the publish
//! must stay well below the bytes published.

use std::{fs, io::Write};

use anyhow::{Result, ensure};

use super::{CaseContext, CaseOutput, Recorder, server::Server, workspace::Workspace};
use crate::{measure::PhasePeak, path_text};

pub(crate) const NAME: &str = "publish_content";
const MIB: usize = 1024 * 1024;
/// Budget for the server's peak while it receives the full-size publish of 384 MiB:
/// one 64 MiB batch, the store's copies while writing it, the blob being reassembled,
/// and the server itself. Holding the whole publish, as before, cannot fit. Measured
/// on 2026-09-26 (release): 210–214 MiB, and 182 MiB for twice the files, so the peak
/// does not grow with the publish.
const SERVER_PEAK_BUDGET_BYTES: f64 = 256.0 * MIB as f64;
/// Budget for the downloading client's peak. Measured on 2026-09-26 (release):
/// 186 MiB, and 154 MiB for twice the files.
const CLONE_PEAK_BUDGET_BYTES: f64 = 256.0 * MIB as f64;

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let files = context.scale.pick(32, 2);
    let file_mib = context.scale.pick(12, 5);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("files", files);
    recorder.parameter("file_mib", file_mib);

    // The server serves the store of a freshly initialized workspace.
    let source = Workspace::new(context)?;
    let server = Server::start(source.root())?;
    let url = server.url();

    let writer = context.scratch.join("writer");
    source.ok_in(&context.scratch, &["clone", url, path_text(&writer)?])?;
    source.ok_in(&writer, &["change", "new", "--message", "large files"])?;
    let paths = (0..files)
        .map(|index| format!("data/f{index:03}.bin"))
        .collect::<Vec<_>>();
    for (index, path) in paths.iter().enumerate() {
        write_noise(&writer.join(path), index, file_mib * MIB)?;
    }
    let mut add = vec!["add"];
    add.extend(paths.iter().map(String::as_str));
    recorder.timing("add", source.ok_in(&writer, &add)?.timing);

    let mut peak = PhasePeak::start()?;
    let publish = source.ok_in(&writer, &["publish"])?;
    recorder.peak_rss("publish_server", peak.take()?);
    recorder.timing("publish", publish.timing);
    recorder.at_most("publish_server.peak_rss", SERVER_PEAK_BUDGET_BYTES, "bytes");

    let reader = context.scratch.join("reader");
    let clone = source.ok_in(&context.scratch, &["clone", url, path_text(&reader)?])?;
    recorder.peak_rss("clone_server", peak.take()?);
    recorder.timing("clone", clone.timing);
    recorder.at_most("clone.peak_rss", CLONE_PEAK_BUDGET_BYTES, "bytes");

    let last = files - 1;
    let mut expected = Vec::new();
    write_noise_into(&mut expected, last, file_mib * MIB)?;
    ensure!(
        fs::read(reader.join(&paths[last]))? == expected,
        "the clone's {} differs from the published file",
        paths[last]
    );

    server.stop()?;
    Ok(recorder.finish())
}

fn write_noise(path: &std::path::Path, seed: usize, len: usize) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    write_noise_into(&mut file, seed, len)?;
    file.flush()?;
    Ok(())
}

/// Bytes that neither repeat nor compress, different for every seed.
fn write_noise_into(out: &mut impl Write, seed: usize, len: usize) -> Result<()> {
    let mut state =
        0x9e37_79b9_7f4a_7c15_u64 ^ (seed as u64 + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let mut block = [0_u8; 8];
    let mut left = len;
    while left > 0 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        block.copy_from_slice(&state.to_le_bytes());
        let take = left.min(block.len());
        out.write_all(&block[..take])?;
        left -= take;
    }
    Ok(())
}
