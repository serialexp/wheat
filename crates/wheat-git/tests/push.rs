use std::{fs, path::Path, process::Command};

use wheat_git::push::{
    PushError, PushMode, RefStatus, RemoteState, cache_observed_ref, observe_remote_ref, push_ref,
};

fn git(directory: Option<&Path>, args: &[&str]) -> String {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    let output = command
        .args(args)
        .env("GIT_AUTHOR_NAME", "Wheat Test")
        .env("GIT_AUTHOR_EMAIL", "wheat@example.invalid")
        .env("GIT_COMMITTER_NAME", "Wheat Test")
        .env("GIT_COMMITTER_EMAIL", "wheat@example.invalid")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn init_bare(path: &Path) {
    git(None, &["init", "--bare", path.to_str().unwrap()]);
}

fn commit(repo: &Path, name: &str, contents: &str) -> String {
    fs::write(repo.join(name), contents).unwrap();
    git(Some(repo), &["add", name]);
    git(Some(repo), &["commit", "-m", name]);
    git(Some(repo), &["rev-parse", "HEAD"])
}

struct Fixture {
    _root: tempfile::TempDir,
    work: std::path::PathBuf,
    projection: std::path::PathBuf,
    remote: std::path::PathBuf,
    root_oid: String,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let work = root.path().join("work");
        let projection = root.path().join("projection.git");
        let remote = root.path().join("remote.git");
        git(None, &["init", "-b", "main", work.to_str().unwrap()]);
        let root_oid = commit(&work, "root", "root\n");
        init_bare(&remote);
        Self {
            _root: root,
            work,
            projection,
            remote,
            root_oid,
        }
    }

    fn snapshot_projection(&self) {
        init_bare(&self.projection);
        let refspec = format!(
            "{}:refs/heads/main",
            git(Some(&self.work), &["rev-parse", "main"])
        );
        git(
            Some(&self.projection),
            &["fetch", "--no-tags", self.work.to_str().unwrap(), &refspec],
        );
    }

    fn remote_url(&self) -> &str {
        self.remote.to_str().unwrap()
    }

    fn seed_remote(&self, oid: &str) {
        let refspec = format!("{oid}:refs/heads/main");
        git(Some(&self.work), &["push", self.remote_url(), &refspec]);
    }
}

#[test]
fn absent_destination_push_succeeds() {
    let fixture = Fixture::new();
    fixture.snapshot_projection();

    let observed = observe_remote_ref(fixture.remote_url(), "refs/heads/main").unwrap();
    assert_eq!(observed, RemoteState::Absent);
    assert_eq!(
        cache_observed_ref(
            &fixture.projection,
            fixture.remote_url(),
            "refs/heads/main",
            &observed
        )
        .unwrap(),
        None
    );

    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::Normal,
    )
    .unwrap();
    assert!(report.accepted(), "{}", report.stderr.to_string_lossy());
    assert_eq!(report.refs[0].status, RefStatus::Created);
}

#[test]
fn absent_exact_lease_rejects_a_concurrent_branch_creation() {
    let fixture = Fixture::new();
    commit(&fixture.work, "local", "local\n");
    fixture.snapshot_projection();
    let observed = observe_remote_ref(fixture.remote_url(), "refs/heads/main").unwrap();
    assert_eq!(observed, RemoteState::Absent);

    let competitor = fixture.root_oid.clone();
    fixture.seed_remote(&competitor);
    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::ForceWithLeaseAbsent,
    )
    .unwrap();
    assert!(!report.accepted());
    assert_eq!(report.refs[0].status, RefStatus::Rejected);
    assert_eq!(
        git(Some(&fixture.remote), &["rev-parse", "refs/heads/main"]),
        competitor
    );
}

#[test]
fn absent_exact_lease_succeeds_while_the_destination_remains_absent() {
    let fixture = Fixture::new();
    fixture.snapshot_projection();
    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::ForceWithLeaseAbsent,
    )
    .unwrap();
    assert!(report.accepted(), "{}", report.stderr.to_string_lossy());
    assert_eq!(report.refs[0].status, RefStatus::Created);
}

#[test]
fn fast_forward_push_succeeds() {
    let fixture = Fixture::new();
    let root_oid = fixture.root_oid.clone();
    fixture.seed_remote(&root_oid);
    let tip = commit(&fixture.work, "local", "local\n");
    fixture.snapshot_projection();

    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::Normal,
    )
    .unwrap();
    assert!(report.accepted(), "{}", report.stderr.to_string_lossy());
    assert_eq!(
        git(Some(&fixture.remote), &["rev-parse", "refs/heads/main"]),
        tip
    );
}

#[test]
fn a_push_that_never_reaches_the_ref_update_reports_gits_failure() {
    // Stands in for a remote that refuses before ref negotiation (an HTTP 401
    // with no credentials, an unreachable host): git prints no porcelain line
    // and the only account of why is its exit status and stderr.
    let fixture = Fixture::new();
    fixture.snapshot_projection();
    let missing = fixture.remote.with_file_name("missing.git");

    let error = push_ref(
        &fixture.projection,
        missing.to_str().unwrap(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::Normal,
    )
    .unwrap_err();
    match &error {
        PushError::GitFailed {
            operation,
            status,
            stderr,
        } => {
            assert_eq!(*operation, "push");
            assert!(status.is_some_and(|code| code != 0), "{status:?}");
            assert!(stderr.contains("missing.git"), "{stderr}");
        }
        other => panic!("expected GitFailed, got {other:?}"),
    }
    let message = error.to_string();
    assert!(
        message.starts_with("git push failed (exit status "),
        "{message}"
    );
    assert!(message.contains("missing.git"), "{message}");
}

fn divergent_fixture() -> (Fixture, RemoteState, String) {
    let fixture = Fixture::new();
    git(Some(&fixture.work), &["branch", "remote-line"]);
    let local_tip = commit(&fixture.work, "local", "local\n");
    git(Some(&fixture.work), &["checkout", "remote-line"]);
    let remote_tip = commit(&fixture.work, "remote", "remote\n");
    fixture.seed_remote(&remote_tip);
    git(Some(&fixture.work), &["checkout", "main"]);
    fixture.snapshot_projection();
    let observed = observe_remote_ref(fixture.remote_url(), "refs/heads/main").unwrap();
    (fixture, observed, local_tip)
}

#[test]
fn divergent_normal_push_is_rejected() {
    let (fixture, _, _) = divergent_fixture();
    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::Normal,
    )
    .unwrap();
    assert!(!report.accepted());
    assert_eq!(report.refs.len(), 1);
    assert_eq!(report.refs[0].status, RefStatus::Rejected);
}

#[test]
fn divergent_push_with_exact_lease_succeeds() {
    let (fixture, observed, local_tip) = divergent_fixture();
    let cached = cache_observed_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        &observed,
    )
    .unwrap();
    assert_eq!(
        cached.as_deref(),
        Some("refs/wheat/remote-cache/heads/main")
    );
    let RemoteState::Present(observed_oid) = observed else {
        panic!("destination must exist")
    };

    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::ForceWithLease(observed_oid),
    )
    .unwrap();
    assert!(report.accepted(), "{}", report.stderr.to_string_lossy());
    assert_eq!(report.refs[0].status, RefStatus::Forced);
    assert_eq!(
        git(Some(&fixture.remote), &["rev-parse", "refs/heads/main"]),
        local_tip
    );
}

#[test]
fn stale_exact_lease_is_rejected() {
    let (fixture, observed, _) = divergent_fixture();
    let RemoteState::Present(observed_oid) = observed else {
        panic!("destination must exist")
    };

    git(Some(&fixture.work), &["checkout", "remote-line"]);
    let advanced = commit(&fixture.work, "advanced", "advanced\n");
    fixture.seed_remote(&advanced);

    let report = push_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        "refs/heads/main",
        PushMode::ForceWithLease(observed_oid),
    )
    .unwrap();
    assert!(!report.accepted());
    assert_eq!(report.refs[0].status, RefStatus::Rejected);
    assert!(report.refs[0].summary.contains("stale info"));
}

#[test]
fn destination_requires_a_valid_full_heads_ref() {
    let fixture = Fixture::new();
    fixture.snapshot_projection();
    for invalid in [
        "main",
        "refs/tags/main",
        "refs/heads/",
        "refs/heads/bad..name",
    ] {
        assert!(matches!(
            push_ref(
                &fixture.projection,
                fixture.remote_url(),
                "refs/heads/main",
                invalid,
                PushMode::Normal
            ),
            Err(PushError::InvalidDestination(reference)) if reference == invalid
        ));
    }
}

#[test]
fn observing_and_caching_do_not_persist_remote_url() {
    let fixture = Fixture::new();
    let root_oid = fixture.root_oid.clone();
    fixture.seed_remote(&root_oid);
    fixture.snapshot_projection();
    let observed = observe_remote_ref(fixture.remote_url(), "refs/heads/main").unwrap();
    cache_observed_ref(
        &fixture.projection,
        fixture.remote_url(),
        "refs/heads/main",
        &observed,
    )
    .unwrap();

    let output = Command::new("git")
        .arg("-C")
        .arg(&fixture.projection)
        .args(["config", "--get-regexp", r"^remote\..*\.url$"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let config = fs::read_to_string(fixture.projection.join("config")).unwrap();
    assert!(!config.contains(fixture.remote_url()));
    let fetch_head = fs::read_to_string(fixture.projection.join("FETCH_HEAD")).unwrap_or_default();
    assert!(!fetch_head.contains(fixture.remote_url()));
}
