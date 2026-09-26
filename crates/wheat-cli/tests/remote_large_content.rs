#![cfg(target_os = "linux")]

//! Content that does not fit one protocol message goes through `wheat publish` and
//! `wheat clone`: a file larger than a message is uploaded and downloaded in chunks,
//! and more new files than one `ContentNeeded` may list all still arrive.

mod remote_support;

use std::fs;
use std::path::{Path, PathBuf};

use remote_support::{git, ok, ok_json, serve, write_config};

/// Bytes that do not repeat, so nothing about the transfer depends on them
/// compressing well.
fn noise(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

/// Commits `files` to Git, imports that into a Wheat workspace, publishes it to a
/// fresh in-process server and clones it back. Returns the clone's directory.
///
/// Files go in through a Git import because `wheat add` stops at 16 MiB.
fn publish_and_clone(root: &Path, files: &[(String, Vec<u8>)]) -> PathBuf {
    let config = root.join("config");
    write_config(&config);
    let server = serve(root, &config);
    let url = &server.url;

    let git_work = root.join("git-work");
    fs::create_dir_all(&git_work).unwrap();
    git(&git_work, &["init", "-q", "-b", "main"]);
    for (path, data) in files {
        let path = git_work.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }
    git(&git_work, &["add", "-A"]);
    git(&git_work, &["commit", "-q", "-m", "content"]);

    let writer_stores = root.join("writer-stores");
    let writer = root.join("writer");
    ok(
        root,
        &writer_stores,
        &config,
        &[
            "import",
            git_work.to_str().unwrap(),
            writer.to_str().unwrap(),
        ],
    );
    // The imported history is unrelated to the server's, hence --force.
    ok(
        &writer,
        &writer_stores,
        &config,
        &["publish", "--remote", url, "--force"],
    );

    let reader_stores = root.join("reader-stores");
    let reader = root.join("reader");
    let cloned = ok_json(
        root,
        &reader_stores,
        &config,
        &["clone", url, reader.to_str().unwrap()],
    );
    // The server has the whole publish, so its push is the whole clone.
    assert_eq!(cloned["pulled"], 0, "{cloned}");
    ok(&reader, &reader_stores, &config, &["validate"]);
    reader
}

fn assert_cloned(reader: &Path, files: &[(String, Vec<u8>)]) {
    for (path, data) in files {
        let cloned = fs::read(reader.join(path)).unwrap();
        assert!(
            cloned == *data,
            "cloned {path} differs from the published one"
        );
    }
}

#[test]
fn a_file_larger_than_a_message_publishes_and_clones() {
    let root = tempfile::tempdir().unwrap();
    // Past the 16 MiB message limit, and not a whole number of 4 MiB chunks.
    let files = [("big.bin".to_owned(), noise(17 * 1024 * 1024 + 5))];
    let reader = publish_and_clone(root.path(), &files);
    assert_cloned(&reader, &files);
}

#[test]
fn more_new_files_than_one_content_request_publish_and_clone() {
    let root = tempfile::tempdir().unwrap();
    let count = wheat_protocol::MAX_WANT_IDS + 5;
    let files = (0..count)
        .map(|index| {
            (
                format!("d{:02}/f{index:05}.txt", index / 1_000),
                format!("file {index}\n").into_bytes(),
            )
        })
        .collect::<Vec<_>>();
    let reader = publish_and_clone(root.path(), &files);
    assert_cloned(&reader, &files);
}
