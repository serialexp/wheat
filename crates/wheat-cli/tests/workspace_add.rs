use std::{
    fs,
    process::{Command, Output},
};

fn wheat_command(root: &std::path::Path, arguments: &[&str]) -> Command {
    let mut adjusted = arguments.to_vec();
    if adjusted.first() == Some(&"init") && adjusted.len() == 1 {
        adjusted.push(root.to_str().unwrap());
    }
    let is_new = adjusted.starts_with(&["change", "new"]);
    let is_add = adjusted.first() == Some(&"add");
    if (is_new || is_add)
        && !adjusted
            .iter()
            .any(|argument| *argument == "--message" || *argument == "-m")
    {
        adjusted.extend(["--message", "test revision"]);
    }
    let config_home = root.parent().unwrap().join(format!(
        ".{}-test-config",
        root.file_name().unwrap().to_string_lossy()
    ));
    let config_directory = config_home.join("wheat");
    std::fs::create_dir_all(&config_directory).unwrap();
    std::fs::write(
        config_directory.join("config.json"),
        r#"{"format":"wheat-config","version":1,"actor":{"name":"Wheat Test","email":"wheat@example.invalid"}}"#,
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_wheat"));
    command
        .arg("--workspace")
        .arg(root)
        .args(adjusted)
        .env(
            "WHEAT_STORE_HOME",
            root.parent().unwrap().join(format!(
                ".{}-test-stores",
                root.file_name().unwrap().to_string_lossy()
            )),
        )
        .env("XDG_CONFIG_HOME", config_home);
    command
}

fn wheat(root: &std::path::Path, arguments: &[&str]) -> Output {
    wheat_command(root, arguments)
        .output()
        .expect("wheat command runs")
}
fn store_path(root: &std::path::Path) -> std::path::PathBuf {
    let home = root.parent().unwrap().join(format!(
        ".{}-test-stores",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::read_dir(home)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
}

fn ok(root: &std::path::Path, arguments: &[&str]) -> Output {
    let output = wheat(root, arguments);
    assert!(
        output.status.success(),
        "command {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn field(output: &Output, name: &str) -> String {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name} ")))
        .unwrap()
        .to_owned()
}

fn ids(output: &Output, name: &str) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .map(|line| line.split_whitespace().map(str::to_owned).collect())
        .unwrap()
}

fn subsumed(output: &Output) -> Vec<(String, String)> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let words = line.split_whitespace().collect::<Vec<_>>();
            (words.len() == 4 && words[0] == "subsumed" && words[2] == "->")
                .then(|| (words[1].to_owned(), words[3].to_owned()))
        })
        .collect()
}

fn edit_ids(output: &Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let id = words.next()?;
            (words.next() == Some("file")).then(|| id.to_owned())
        })
        .collect()
}

fn setup() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["init"]);
    let base = ok(
        dir.path(),
        &[
            "change",
            "new",
            "--path",
            "file",
            "--value",
            "top\nfoo\nbottom\n",
        ],
    );
    let child = ok(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&base, "change"),
            "--path",
            "file",
            "--expected",
            "top\nfoo\nbottom\n",
            "--value",
            "top\nbar\nbottom\n",
        ],
    );
    (dir, field(&child, "change"), field(&child, "revision"))
}

#[test]
fn repeated_add_normalizes_against_retained_authored_base_and_supports_undo() {
    let (dir, change, first) = setup();
    fs::write(dir.path().join("file"), "top\nbaz\nbottom\n").unwrap();
    let second = ok(
        dir.path(),
        &["add", "--change", &change, "--predecessor", &first, "file"],
    );
    let second_revision = field(&second, "revision");
    let shown = ok(dir.path(), &["edits", "--revision", &second_revision]);
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(text.contains("guarded-text"));
    assert!(text.contains("transition preserved="));

    // Reopen through a new process and prove all guarded fragments were retained.
    fs::write(dir.path().join("file"), "top\nfoo\nbottom\n").unwrap();
    let undone = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &second_revision,
            "file",
        ],
    );
    let undo_revision = field(&undone, "revision");
    let shown = ok(dir.path(), &["edits", "--revision", &undo_revision]);
    assert!(String::from_utf8_lossy(&shown.stdout).contains("transition"));
}

#[test]
fn independent_guarded_components_have_exact_lineage_and_undo_has_none() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["init"]);
    let base_text = "zero\none\ntwo\nthree\nfour\n";
    let first_text = "zero\nONE\ntwo\nthree\nfour\n";
    let second_text = "zero\nONE\ntwo\nthree\nFOUR\n";
    let workspace = "zero\nUNO\ntwo\nthree\nCUATRO\n";
    let base = ok(
        dir.path(),
        &["change", "new", "--path", "file", "--value", base_text],
    );
    let first = ok(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&base, "change"),
            "--path",
            "file",
            "--expected",
            base_text,
            "--value",
            first_text,
        ],
    );
    // Add the second authored fragment through the workspace so the predecessor
    // contains two independent guarded edits on this one path.
    fs::write(dir.path().join("file"), second_text).unwrap();
    let second = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &field(&first, "change"),
            "--predecessor",
            &field(&first, "revision"),
            "--guard",
            "file:19:24",
            "file",
        ],
    );
    let change = field(&first, "change");
    let predecessor = field(&second, "revision");
    let old = ok(dir.path(), &["edits", "--revision", &predecessor]);
    let old_ids = edit_ids(&old);
    assert_eq!(old_ids.len(), 2);

    fs::write(dir.path().join("file"), workspace).unwrap();
    let added = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "file",
        ],
    );
    let revision = field(&added, "revision");
    let shown = ok(dir.path(), &["edits", "--revision", &revision]);
    let replacement_ids = edit_ids(&shown);
    assert_eq!(replacement_ids.len(), 2);
    assert_eq!(ids(&shown, "preserved"), Vec::<String>::new());
    assert_eq!(ids(&shown, "removed"), old_ids);
    assert_eq!(ids(&shown, "added"), replacement_ids);
    assert_eq!(
        subsumed(&shown),
        vec![
            (
                ids(&shown, "removed")[0].clone(),
                ids(&shown, "added")[0].clone()
            ),
            (
                ids(&shown, "removed")[1].clone(),
                ids(&shown, "added")[1].clone()
            ),
        ]
    );

    fs::write(dir.path().join("file"), base_text).unwrap();
    let undone = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &revision,
            "file",
        ],
    );
    let undo = ok(
        dir.path(),
        &["edits", "--revision", &field(&undone, "revision")],
    );
    assert_eq!(ids(&undo, "removed"), replacement_ids);
    assert_eq!(ids(&undo, "added"), Vec::<String>::new());
    assert!(subsumed(&undo).is_empty());

    fs::write(dir.path().join("file"), workspace).unwrap();
    let reintroduced = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &field(&undone, "revision"),
            "file",
        ],
    );
    let reintroduced = ok(
        dir.path(),
        &["edits", "--revision", &field(&reintroduced, "revision")],
    );
    assert_eq!(ids(&reintroduced, "added"), replacement_ids);
    assert!(subsumed(&reintroduced).is_empty());
}

#[test]
fn only_selected_paths_are_read_and_workspace_is_never_overwritten() {
    let (dir, change, predecessor) = setup();
    fs::write(dir.path().join("file"), "top\nbaz\nbottom\n").unwrap();
    fs::write(dir.path().join("dirty"), b"do not touch").unwrap();
    ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "file",
        ],
    );
    assert_eq!(
        fs::read(dir.path().join("file")).unwrap(),
        b"top\nbaz\nbottom\n"
    );
    assert_eq!(fs::read(dir.path().join("dirty")).unwrap(), b"do not touch");
}

#[test]
fn stale_or_invalid_add_leaves_published_head_unchanged() {
    let (dir, change, predecessor) = setup();
    fs::write(dir.path().join("file"), "top\nbaz\nbottom\n").unwrap();
    let head_path = store_path(dir.path()).join("repository.head");
    let head = fs::read(&head_path).unwrap();
    let failed = wheat(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "file",
        ],
    );
    assert!(!failed.status.success());
    assert_eq!(fs::read(&head_path).unwrap(), head);

    let failed = wheat(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "--guard",
            "file:999:1000",
            "file",
        ],
    );
    assert!(!failed.status.success());
    assert_eq!(fs::read(&head_path).unwrap(), head);
}

#[test]
fn add_recovers_pending_restore_before_capturing_workspace_bytes() {
    let (dir, change, predecessor) = setup();
    ok(dir.path(), &["materialize", "--source", "working"]);
    fs::write(dir.path().join("file"), "discarded workspace bytes\n").unwrap();
    let interrupted = wheat_command(dir.path(), &["restore", "file"])
        .env(
            "WHEAT_TEST_MATERIALIZATION_FAIL_AFTER",
            "completion-persisted",
        )
        .env("WHEAT_TEST_MATERIALIZATION_FAIL_MODE", "sigkill")
        .output()
        .unwrap();
    assert!(!interrupted.status.success());
    assert!(dir.path().join(".wheat/materialization.json").exists());

    let added = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "file",
        ],
    );
    ok(
        dir.path(),
        &["edits", "--revision", &field(&added, "revision")],
    );
    assert_eq!(
        fs::read(dir.path().join("file")).unwrap(),
        b"top\nbar\nbottom\n"
    );
    assert!(!dir.path().join(".wheat/materialization.json").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn add_holds_the_workspace_operation_lock_across_capture_and_publication() {
    use std::os::fd::AsRawFd as _;

    let (dir, change, predecessor) = setup();
    fs::write(dir.path().join("file"), "top\nbaz\nbottom\n").unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join(".wheat/workspace.lock"))
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let head_path = store_path(dir.path()).join("repository.head");
    let head = fs::read(&head_path).unwrap();

    let blocked = wheat(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "file",
        ],
    );
    assert!(!blocked.status.success());
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("workspace is locked"));
    assert_eq!(fs::read(head_path).unwrap(), head);
}

#[test]
fn explicit_guard_ranges_use_canonical_lf_byte_offsets() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["init"]);
    let base_text = "zero\none\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    let child_text = "zero\nONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    let workspace = "zero\nONE\ntwo\nthree\nfour\nfive\nsix\nseven\nEXTRA\neight\nnine\n";
    let base = ok(
        dir.path(),
        &["change", "new", "--path", "file", "--value", base_text],
    );
    let child = ok(
        dir.path(),
        &[
            "change",
            "new",
            "--depends-on",
            &field(&base, "change"),
            "--path",
            "file",
            "--expected",
            base_text,
            "--value",
            child_text,
        ],
    );
    let change = field(&child, "change");
    let predecessor = field(&child, "revision");
    // Coordinates are bytes in canonical LF content. The explicit range includes
    // the inserted `EXTRA` line and uniquely mapped neighboring context (33..50).
    fs::write(dir.path().join("file"), workspace).unwrap();
    let added = ok(
        dir.path(),
        &[
            "add",
            "--change",
            &change,
            "--predecessor",
            &predecessor,
            "--guard",
            "file:33:49",
            "file",
        ],
    );
    let shown = ok(
        dir.path(),
        &["edits", "--revision", &field(&added, "revision")],
    );
    assert!(String::from_utf8_lossy(&shown.stdout).contains("guarded-text"));
}
