#![cfg(target_os = "linux")]

//! Status and the workspace's stat cache: an unchanged file is not read again, a
//! changed stat is, and a long binary file found modified by its samples is not read
//! whole unless its content ID is asked for.

use std::{
    fs,
    io::{Seek, SeekFrom, Write},
    path::Path,
    process::Command,
    time::{Duration, SystemTime},
};

use wheat_client::{
    workspace_scan::{ContentIds, ScanReads},
    workspace_status::{self, WorkspaceStatus},
};

fn ok(root: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new(env!("CARGO_BIN_EXE_wheat"))
        .current_dir(root)
        .env("XDG_CONFIG_HOME", config)
        .env("WHEAT_STORE_HOME", config.join("stores"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A workspace whose one change adds `files`, each written an hour ago.
fn workspace(files: &[(&str, &[u8])]) -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let (path, home) = (root.path(), config.path());
    ok(path, home, &["init"]);
    ok(path, home, &["config", "set", "actor.name", "Test"]);
    ok(
        path,
        home,
        &["config", "set", "actor.email", "test@example.invalid"],
    );
    fs::write(
        path.join(".wheat.json"),
        r#"{"format":"wheat-repository","version":1,"max_file_bytes":1073741824}"#,
    )
    .unwrap();
    ok(path, home, &["change", "new", "-m", "files"]);
    let mut arguments = vec!["add", ".wheat.json"];
    for (name, bytes) in files {
        fs::write(path.join(name), bytes).unwrap();
        set_mtime(&path.join(name), hours_ago(1));
        arguments.push(name);
    }
    ok(path, home, &arguments);
    set_mtime(&path.join(".wheat.json"), hours_ago(1));
    (root, config)
}

fn hours_ago(hours: u64) -> SystemTime {
    SystemTime::now() - Duration::from_secs(hours * 3600)
}

fn set_mtime(path: &Path, time: SystemTime) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

fn status(root: &Path, ids: ContentIds) -> WorkspaceStatus {
    workspace_status::read(root, None, ids).unwrap()
}

fn changed(status: &WorkspaceStatus) -> Vec<&str> {
    status
        .changes
        .iter()
        .map(|change| change.path.as_str())
        .collect()
}

fn reads(files_read: usize, files_sampled: usize, baselines_read: usize) -> (usize, usize, usize) {
    (files_read, files_sampled, baselines_read)
}

fn counted(reads: ScanReads) -> (usize, usize, usize) {
    (reads.files_read, reads.files_sampled, reads.baselines_read)
}

#[test]
fn status_reads_only_files_whose_stat_changed() {
    let (root, _config) = workspace(&[("a", b"one\n"), ("b", b"two\n"), ("c", b"three\n")]);
    let root = root.path();

    let first = status(root, ContentIds::Required);
    assert_eq!(changed(&first), Vec::<&str>::new());
    assert_eq!(counted(first.reads), reads(4, 0, 4));
    // Nothing changed: nothing is read, not even the baselines.
    let second = status(root, ContentIds::Required);
    assert_eq!(changed(&second), Vec::<&str>::new());
    assert_eq!(counted(second.reads), reads(0, 0, 0));

    // A modified file is read again, then known from its stat while it stays so.
    fs::write(root.join("b"), "TWO\n").unwrap();
    set_mtime(&root.join("b"), hours_ago(1));
    let modified = status(root, ContentIds::Required);
    assert_eq!(changed(&modified), ["b"]);
    assert_eq!(counted(modified.reads), reads(1, 0, 1));
    let again = status(root, ContentIds::Required);
    assert_eq!(changed(&again), ["b"]);
    assert_eq!(counted(again.reads), reads(0, 0, 1));
    assert_eq!(
        again.changes[0].workspace.as_ref().unwrap().content,
        modified.changes[0].workspace.as_ref().unwrap().content
    );

    // Same length, mtime set back: the ctime still changed.
    let before = fs::metadata(root.join("c")).unwrap().modified().unwrap();
    fs::write(root.join("c"), "THREE\n").unwrap();
    set_mtime(&root.join("c"), before);
    assert_eq!(changed(&status(root, ContentIds::Required)), ["b", "c"]);

    // Put back: unchanged again.
    fs::write(root.join("b"), "two\n").unwrap();
    fs::write(root.join("c"), "three\n").unwrap();
    assert_eq!(
        changed(&status(root, ContentIds::Required)),
        Vec::<&str>::new()
    );
    fs::remove_file(root.join("a")).unwrap();
    let deleted = status(root, ContentIds::Required);
    assert_eq!(changed(&deleted), ["a"]);
    assert!(deleted.changes[0].deleted);
}

/// A file whose mtime is not older than the read's start may still be written in the
/// same instant, so it is read every time until it is older.
#[test]
fn a_file_written_after_the_read_started_is_not_trusted() {
    let (root, _config) = workspace(&[("a", b"one\n")]);
    let root = root.path();
    set_mtime(
        &root.join("a"),
        SystemTime::now() + Duration::from_secs(3600),
    );
    status(root, ContentIds::Required);
    assert_eq!(status(root, ContentIds::Required).reads.files_read, 1);
    set_mtime(&root.join("a"), hours_ago(1));
    status(root, ContentIds::Required);
    assert_eq!(status(root, ContentIds::Required).reads.files_read, 0);
}

#[test]
fn an_unreadable_stat_cache_is_emptied() {
    let (root, _config) = workspace(&[("a", b"one\n")]);
    let root = root.path();
    status(root, ContentIds::Required);
    fs::write(
        root.join(".wheat/stat.sqlite"),
        b"not a database at all, sorry",
    )
    .unwrap();
    let after = status(root, ContentIds::Required);
    assert_eq!(changed(&after), Vec::<&str>::new());
    assert_eq!(after.reads.files_read, 2);
    assert_eq!(status(root, ContentIds::Required).reads.files_read, 0);
}

/// 17 MiB of noise: binary, and past the length that gets samples.
fn noise() -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    (0..17 << 20)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[test]
fn a_long_file_whose_samples_changed_is_modified_without_reading_it() {
    let long = noise();
    let (root, config) = workspace(&[("long.bin", &long)]);
    let (root, config) = (root.path(), config.path());
    let path = root.join("long.bin");
    // The first status reads it whole and keeps its samples.
    assert_eq!(status(root, ContentIds::Required).reads.files_read, 2);

    // A byte at the first sample changes, the length does not.
    let mut file = fs::File::options().write(true).open(&path).unwrap();
    file.write_all(&[!long[0]]).unwrap();
    drop(file);
    set_mtime(&path, hours_ago(1));
    let sampled = status(root, ContentIds::Optional);
    assert_eq!(changed(&sampled), ["long.bin"]);
    assert_eq!(counted(sampled.reads), reads(0, 1, 1));
    assert!(sampled.changes[0].workspace.is_none());
    assert!(!sampled.changes[0].deleted);
    let human = String::from_utf8(ok(root, config, &["status"]).stdout).unwrap();
    assert!(human.contains("modified long.bin"), "{human}");
    // Asked for content IDs, status reads it whole.
    let identified = status(root, ContentIds::Required);
    assert_eq!(identified.reads.files_read, 1);
    assert!(identified.changes[0].workspace.is_some());

    // Put back, and a length change needs no samples.
    fs::write(&path, &long).unwrap();
    set_mtime(&path, hours_ago(1));
    assert_eq!(
        changed(&status(root, ContentIds::Required)),
        Vec::<&str>::new()
    );
    let mut file = fs::File::options().append(true).open(&path).unwrap();
    file.write_all(b"more").unwrap();
    drop(file);
    set_mtime(&path, hours_ago(1));
    let longer = status(root, ContentIds::Optional);
    assert_eq!(changed(&longer), ["long.bin"]);
    assert_eq!(counted(longer.reads), reads(0, 0, 1));

    // Put back again; a change between samples, or a touch, matches the samples, so
    // the file is read whole.
    fs::write(&path, &long).unwrap();
    set_mtime(&path, hours_ago(1));
    status(root, ContentIds::Required);
    let mut file = fs::File::options().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(4096 + 1)).unwrap();
    file.write_all(&[!long[4097]]).unwrap();
    drop(file);
    set_mtime(&path, hours_ago(1));
    let between = status(root, ContentIds::Optional);
    assert_eq!(changed(&between), ["long.bin"]);
    assert_eq!(counted(between.reads), reads(1, 1, 1));
    assert!(between.changes[0].workspace.is_some());
    fs::write(&path, &long).unwrap();
    set_mtime(&path, hours_ago(1));
    status(root, ContentIds::Required);
    set_mtime(&path, hours_ago(2));
    let touched = status(root, ContentIds::Optional);
    assert_eq!(changed(&touched), Vec::<&str>::new());
    assert_eq!(counted(touched.reads), reads(1, 1, 1));
}
