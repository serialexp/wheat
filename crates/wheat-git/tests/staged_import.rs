use std::{
    path::Path,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use wheat_core::{EditOperation, ObjectId, RevisionId, ViewId};
use wheat_git::{DEFAULT_STAGE_COMMITS, import_repository, stage_import};
use wheat_store::{
    ContentStore, ImmutableObjectStore, LooseObjectStore, ObjectStoreError, PackedObjectStore,
    RepositoryReader, StoredObject,
};

#[derive(Clone, Copy)]
enum BatchFault {
    None,
    Error { batch: usize, after_writes: usize },
    MissingSuccess { batch: usize },
}

struct ObservedStore {
    inner: LooseObjectStore,
    batches: AtomicUsize,
    fault: BatchFault,
}

struct BlockedGetStore {
    inner: LooseObjectStore,
    blocked: ObjectId,
}

impl ObservedStore {
    fn new(path: impl Into<std::path::PathBuf>, fault: BatchFault) -> Self {
        Self {
            inner: LooseObjectStore::new(path),
            batches: AtomicUsize::new(0),
            fault,
        }
    }
}

#[async_trait]
impl ImmutableObjectStore for ObservedStore {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        let batch = self.batches.fetch_add(1, Ordering::SeqCst) + 1;
        match self.fault {
            BatchFault::Error {
                batch: failed,
                after_writes,
            } if batch == failed => {
                for (id, bytes) in objects.iter().take(after_writes) {
                    self.inner.put_if_absent(*id, bytes).await?;
                }
                Err(ObjectStoreError::Backend("injected batch failure".into()))
            }
            // Simulate a backend that acknowledges the write without publishing
            // any of the requested objects. Post-flush authentication must catch it.
            BatchFault::MissingSuccess { batch: failed } if batch == failed => Ok(()),
            _ => self.inner.put_batch(objects, concurrency).await,
        }
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

#[async_trait]
impl ImmutableObjectStore for BlockedGetStore {
    async fn put_if_absent(
        &self,
        id: ObjectId,
        canonical_bytes: &[u8],
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, canonical_bytes).await
    }

    async fn put_batch(
        &self,
        objects: &[(ObjectId, Vec<u8>)],
        concurrency: usize,
    ) -> Result<(), ObjectStoreError> {
        self.inner.put_batch(objects, concurrency).await
    }

    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        if id == self.blocked {
            return Err(ObjectStoreError::Missing(id));
        }
        self.inner.get(id).await
    }

    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn linear_fixture(commits: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    for number in 0..commits {
        std::fs::write(dir.path().join("file"), number.to_string()).unwrap();
        git(dir.path(), &["add", "file"]);
        git(dir.path(), &["commit", "-m", "next"]);
    }
    dir
}

fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    std::fs::write(dir.path().join("file"), b"same").unwrap();
    std::fs::write(dir.path().join("duplicate"), b"same").unwrap();
    std::fs::write(dir.path().join("text"), b"one\r\ntwo\rthree\n").unwrap();
    std::fs::write(dir.path().join("script"), b"#!/bin/sh\r\necho ok\r").unwrap();
    std::fs::write(dir.path().join("binary"), b"nul\0raw\r\n\xff").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["update-index", "--chmod=+x", "script"]);
    let shared_oid = git(dir.path(), &["hash-object", "text"]);
    git(
        dir.path(),
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{shared_oid},link"),
        ],
    );
    git(dir.path(), &["commit", "-m", "root"]);
    git(dir.path(), &["update-index", "--chmod=+x", "text"]);
    git(dir.path(), &["commit", "-m", "chmod only"]);
    git(dir.path(), &["checkout", "-b", "feature"]);
    git(dir.path(), &["rm", "duplicate"]);
    git(dir.path(), &["commit", "-m", "delete duplicate"]);
    git(dir.path(), &["checkout", "main"]);
    std::fs::write(dir.path().join("other"), b"other").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "main"]);
    git(dir.path(), &["merge", "--no-ff", "feature", "-m", "merge"]);
    dir
}

#[tokio::test]
async fn interrupted_staging_matches_reference_commit_identities() {
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");
    let reference = import_repository(git.path(), &content).unwrap();
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    // Probe every canonical view, not just the chosen branch tip. Matching IDs
    // imply equal canonical records and all historical selections/content roots.
    let reader = RepositoryReader::new(&objects, &content);
    for expected in reference.commits.values() {
        let revision = reader.revision(expected.revision_id).await.unwrap();
        assert_eq!(revision.change_id, expected.change_id);
        let view = reader.view(expected.resulting_view).await.unwrap();
        assert_eq!(view.id(), expected.resulting_view);
    }
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    assert!(
        !dir.path().join("repository.head").exists(),
        "staging must not publish"
    );
}

#[tokio::test]
async fn packed_restart_resume_matches_loose_reference_and_never_checkpoints_missing_objects() {
    let git = fixture();
    let directory = tempfile::tempdir().unwrap();
    let journal = directory.path().join("import.sqlite");
    let reference_content = ContentStore::new(directory.path().join("reference-content"));
    let reference = import_repository(git.path(), &reference_content).unwrap();

    {
        let objects = PackedObjectStore::open(directory.path().join("packed")).unwrap();
        let content = objects.content_store();
        stage_import(git.path(), &journal, &objects, &content, Some(1))
            .await
            .unwrap();
        drop(content);
        objects.close().unwrap();
    }
    let objects = PackedObjectStore::open(directory.path().join("packed")).unwrap();
    let content = objects.content_store();
    let resumed = stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    assert!(resumed.complete);

    // The journal cannot be authoritative: every completed mapping must reopen
    // solely from its durable segments and pass typed reads.
    let reader = RepositoryReader::new(&objects, &content);
    for expected in reference.commits.values() {
        assert_eq!(
            reader
                .revision(expected.revision_id)
                .await
                .unwrap()
                .change_id,
            expected.change_id
        );
        assert_eq!(
            reader.view(expected.resulting_view).await.unwrap().id(),
            expected.resulting_view
        );
    }
}

#[tokio::test]
async fn sixteen_independent_guarded_edits_match_direct_import() {
    let git = tempfile::tempdir().unwrap();
    self::git(git.path(), &["init", "-b", "main"]);
    for index in 0..16 {
        std::fs::write(
            git.path().join(format!("file-{index:02}.txt")),
            "before\nshared\n",
        )
        .unwrap();
    }
    self::git(git.path(), &["add", "."]);
    self::git(git.path(), &["commit", "-m", "base"]);
    for index in 0..16 {
        std::fs::write(
            git.path().join(format!("file-{index:02}.txt")),
            format!("after {index}\nshared\n"),
        )
        .unwrap();
    }
    self::git(git.path(), &["add", "."]);
    self::git(git.path(), &["commit", "-m", "sixteen guarded edits"]);

    let direct_content_dir = tempfile::tempdir().unwrap();
    let direct_content = ContentStore::new(direct_content_dir.path());
    let direct = import_repository(git.path(), &direct_content).unwrap();
    let staged_dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(staged_dir.path().join("objects"));
    let staged_content = ContentStore::new(staged_dir.path().join("content"));
    stage_import(
        git.path(),
        staged_dir.path().join("journal.sqlite"),
        &objects,
        &staged_content,
        None,
    )
    .await
    .unwrap();

    let tip = self::git(git.path(), &["rev-parse", "HEAD"]);
    let tip = tip
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let expected = &direct.commits[&tip];
    let reader = RepositoryReader::new(&objects, &staged_content);
    let revision = reader.revision(expected.revision_id).await.unwrap();
    assert_eq!(revision.edits.len(), 16);
    assert_eq!(revision.change_id, expected.change_id);
    assert_eq!(
        reader.view(expected.resulting_view).await.unwrap().id(),
        expected.resulting_view
    );
}

#[tokio::test]
async fn repeated_line_deletion_uses_a_conservative_guarded_envelope() {
    let git = tempfile::tempdir().unwrap();
    self::git(git.path(), &["init", "-b", "main"]);
    std::fs::write(
        git.path().join("repeated.txt"),
        b"head\nrepeat\nrepeat\ntail\n",
    )
    .unwrap();
    self::git(git.path(), &["add", "repeated.txt"]);
    self::git(git.path(), &["commit", "-m", "base"]);
    std::fs::write(git.path().join("repeated.txt"), b"head\nrepeat\ntail\n").unwrap();
    self::git(git.path(), &["add", "repeated.txt"]);
    self::git(git.path(), &["commit", "-m", "ambiguous deletion"]);
    let commit = self::git(git.path(), &["rev-parse", "HEAD"]);

    let output = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(output.path().join("objects"));
    let content = ContentStore::new(output.path().join("content"));
    let staged = stage_import(
        git.path(),
        output.path().join("journal.sqlite"),
        &objects,
        &content,
        None,
    )
    .await
    .unwrap();
    assert!(staged.complete);

    let tip = commit
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let native = staged
        .branches
        .values()
        .find(|branch| branch.source_oid == tip)
        .and_then(|branch| branch.native)
        .unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let revision = reader.revision(native.revision_id).await.unwrap();
    assert_eq!(revision.edits.len(), 1);
    let edit = reader.edit(*revision.edits.first().unwrap()).await.unwrap();
    let EditOperation::GuardedText {
        expected,
        replacement,
        ..
    } = edit.operation
    else {
        panic!("repeated text deletion must remain guarded")
    };
    assert_eq!(content.get(expected).unwrap(), b"repeat\nrepeat\n");
    assert_eq!(content.get(replacement).unwrap(), b"repeat\n");
}

#[tokio::test]
async fn dependent_linear_commits_share_one_bounded_acknowledgement_batch() {
    let git = linear_fixture(8);
    let dir = tempfile::tempdir().unwrap();
    let objects = ObservedStore::new(dir.path().join("objects"), BatchFault::None);
    let content = ContentStore::new(dir.path().join("content"));
    let result = stage_import(
        git.path(),
        dir.path().join("import.sqlite"),
        &objects,
        &content,
        Some(8),
    )
    .await
    .unwrap();

    assert!(result.complete);
    assert_eq!(result.completed_commits, 8);
    assert_eq!(objects.batches.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn failed_batch_flush_completes_nothing_and_resume_is_idempotent() {
    let git = linear_fixture(4);
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("import.sqlite");
    let objects = ObservedStore::new(
        dir.path().join("objects"),
        BatchFault::Error {
            batch: 2,
            after_writes: 1,
        },
    );
    let content = ContentStore::new(dir.path().join("content"));

    assert!(
        stage_import(git.path(), &journal_path, &objects, &content, Some(4))
            .await
            .is_err()
    );
    let completed: i64 = rusqlite::Connection::open(&journal_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM work WHERE status = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(completed, 0);

    let resumed = stage_import(git.path(), &journal_path, &objects, &content, Some(4))
        .await
        .unwrap();
    assert!(resumed.complete);
    assert_eq!(resumed.completed_commits, 4);
    let repeated = stage_import(git.path(), &journal_path, &objects, &content, Some(4))
        .await
        .unwrap();
    assert_eq!(repeated, resumed);
}

#[tokio::test]
async fn invalid_completed_ancestry_prevents_prepared_descendant_promotion() {
    let git = linear_fixture(2);
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("import.sqlite");
    let object_path = dir.path().join("objects");
    let objects = LooseObjectStore::new(&object_path);
    let content = ContentStore::new(dir.path().join("content"));
    stage_import(git.path(), &journal_path, &objects, &content, Some(2))
        .await
        .unwrap();

    let mut connection = rusqlite::Connection::open(&journal_path).unwrap();
    let blocked: Vec<u8> = connection
        .query_row(
            "SELECT revision_id FROM work ORDER BY sequence LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute_batch(
            "INSERT INTO prepared_work(oid, change_id, revision_id, view_id, application_id) \
             SELECT oid, change_id, revision_id, view_id, application_id FROM work ORDER BY sequence DESC LIMIT 1; \
             UPDATE work SET status = 0, change_id = NULL, revision_id = NULL, view_id = NULL, application_id = NULL \
             WHERE sequence = (SELECT MAX(sequence) FROM work);",
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let objects = BlockedGetStore {
        inner: LooseObjectStore::new(object_path),
        blocked: ObjectId::from_digest(blocked.try_into().unwrap()),
    };
    assert!(
        stage_import(git.path(), &journal_path, &objects, &content, Some(0))
            .await
            .is_err()
    );
    let connection = rusqlite::Connection::open(&journal_path).unwrap();
    let child_status: i64 = connection
        .query_row(
            "SELECT status FROM work ORDER BY sequence DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let prepared: i64 = connection
        .query_row("SELECT COUNT(*) FROM prepared_work", [], |row| row.get(0))
        .unwrap();
    assert_eq!(child_status, 0);
    assert_eq!(prepared, 1);
}

#[tokio::test]
async fn nominally_successful_flush_with_a_missing_object_completes_nothing() {
    let git = linear_fixture(2);
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("import.sqlite");
    let objects = ObservedStore::new(
        dir.path().join("objects"),
        BatchFault::MissingSuccess { batch: 2 },
    );
    let content = ContentStore::new(dir.path().join("content"));

    assert!(
        stage_import(git.path(), &journal_path, &objects, &content, Some(2))
            .await
            .is_err()
    );
    let connection = rusqlite::Connection::open(&journal_path).unwrap();
    let completed: i64 = connection
        .query_row("SELECT COUNT(*) FROM work WHERE status = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(completed, 0);
}

#[tokio::test]
async fn packed_resume_authenticates_and_promotes_prepared_mappings_after_reopen() {
    let git = linear_fixture(3);
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("import.sqlite");
    let storage = dir.path().join("packed");

    let staged = {
        let objects = PackedObjectStore::open(&storage).unwrap();
        let content = objects.content_store();
        let staged = stage_import(git.path(), &journal_path, &objects, &content, Some(3))
            .await
            .unwrap();
        assert!(staged.complete);
        drop(content);
        objects.close().unwrap();
        staged
    };

    // Model the durable journal boundary left by a crash after preparation and
    // object flush but before the mappings were promoted to completed work.
    let mut connection = rusqlite::Connection::open(&journal_path).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute_batch(
            "INSERT INTO prepared_work(oid, change_id, revision_id, view_id, application_id) \
             SELECT oid, change_id, revision_id, view_id, application_id FROM work WHERE status = 1; \
             UPDATE work SET status = 0, change_id = NULL, revision_id = NULL, view_id = NULL, application_id = NULL;",
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let objects = PackedObjectStore::open(&storage).unwrap();
    let content = objects.content_store();
    let resumed = stage_import(git.path(), &journal_path, &objects, &content, Some(0))
        .await
        .unwrap();
    assert_eq!(resumed, staged);
    let repeated = stage_import(git.path(), &journal_path, &objects, &content, Some(0))
        .await
        .unwrap();
    assert_eq!(repeated, staged);

    // Authentication and promotion must use reopened packed bytes, rather than
    // invocation-local buffers or the prepared journal rows as authority.
    let connection = rusqlite::Connection::open(&journal_path).unwrap();
    let mappings = {
        let mut statement = connection
            .prepare("SELECT revision_id, view_id FROM work ORDER BY sequence")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    drop(connection);
    let reader = RepositoryReader::new(&objects, &content);
    for (revision, view) in mappings {
        let revision = RevisionId::from_digest(revision.try_into().unwrap());
        let view = ViewId::from_digest(view.try_into().unwrap());
        assert_eq!(reader.revision(revision).await.unwrap().id(), revision);
        assert_eq!(reader.view(view).await.unwrap().id(), view);
    }
}

#[tokio::test]
async fn resume_promotes_fully_durable_prepared_mappings_idempotently() {
    let git = linear_fixture(3);
    let dir = tempfile::tempdir().unwrap();
    let journal_path = dir.path().join("import.sqlite");
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let staged = stage_import(git.path(), &journal_path, &objects, &content, Some(3))
        .await
        .unwrap();
    assert!(staged.complete);

    let mut connection = rusqlite::Connection::open(&journal_path).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute_batch(
            "INSERT INTO prepared_work(oid, change_id, revision_id, view_id, application_id) \
             SELECT oid, change_id, revision_id, view_id, application_id FROM work WHERE status = 1; \
             UPDATE work SET status = 0, change_id = NULL, revision_id = NULL, view_id = NULL, application_id = NULL;",
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let resumed = stage_import(git.path(), &journal_path, &objects, &content, Some(0))
        .await
        .unwrap();
    assert_eq!(resumed, staged);
    let repeated = stage_import(git.path(), &journal_path, &objects, &content, Some(0))
        .await
        .unwrap();
    assert_eq!(repeated, staged);
}

#[tokio::test]
async fn default_stage_budget_leaves_large_history_incomplete_then_resumes() {
    let git = tempfile::tempdir().unwrap();
    self::git(git.path(), &["init", "-b", "main"]);
    for number in 0..=DEFAULT_STAGE_COMMITS {
        std::fs::write(git.path().join("file"), number.to_string()).unwrap();
        self::git(git.path(), &["add", "file"]);
        self::git(git.path(), &["commit", "-m", "next"]);
    }
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");
    let first = stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    assert!(!first.complete);
    assert!(first.pending_commits > 0 || first.discovery_pending);
    let mut result = first;
    while !result.complete {
        result = stage_import(git.path(), &journal, &objects, &content, None)
            .await
            .unwrap();
    }
    assert_eq!(result.completed_commits, DEFAULT_STAGE_COMMITS + 1);
}

#[tokio::test]
async fn oversized_stage_budget_is_capped_and_resumable() {
    let git = linear_fixture(DEFAULT_STAGE_COMMITS + 1);
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");

    let first = stage_import(
        git.path(),
        &journal,
        &objects,
        &content,
        Some(DEFAULT_STAGE_COMMITS + 100),
    )
    .await
    .unwrap();
    assert_eq!(first.completed_commits, DEFAULT_STAGE_COMMITS);
    assert!(!first.complete);

    let resumed = stage_import(
        git.path(),
        &journal,
        &objects,
        &content,
        Some(DEFAULT_STAGE_COMMITS + 100),
    )
    .await
    .unwrap();
    assert!(resumed.complete);
    assert_eq!(resumed.completed_commits, DEFAULT_STAGE_COMMITS + 1);
}

#[tokio::test]
async fn resume_rejects_changed_source_refs() {
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let journal = dir.path().join("import.sqlite");
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    self::git(git.path(), &["commit", "--allow-empty", "-m", "moved"]);
    assert!(
        stage_import(git.path(), &journal, &objects, &content, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn finalization_observer_orders_success_and_stops_on_preflight_failure() {
    use wheat_git::{FinalizePhase, finalize_import_with_observer};
    use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore};

    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let journal = dir.path().join("import.sqlite");

    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    let mut failed_phases = Vec::new();
    assert!(
        finalize_import_with_observer(
            git.path(),
            &journal,
            &objects,
            &content,
            &heads,
            "refs/heads/main",
            |phase| failed_phases.push(phase),
        )
        .await
        .is_err()
    );
    assert_eq!(failed_phases, vec![FinalizePhase::Preflight]);
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));

    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    let mut phases = Vec::new();
    finalize_import_with_observer(
        git.path(),
        &journal,
        &objects,
        &content,
        &heads,
        "refs/heads/main",
        |phase| phases.push(phase),
    )
    .await
    .unwrap();
    assert_eq!(
        phases,
        vec![
            FinalizePhase::Preflight,
            FinalizePhase::JournalAuthenticationAccumulation,
            FinalizePhase::AggregateSetConstruction,
            FinalizePhase::BatchFlush,
            FinalizePhase::PrepareEnvelopeValidation,
            FinalizePhase::HeadInitialize,
        ]
    );
}

#[tokio::test]
async fn finalization_retains_branches_without_journal_and_refuses_partial_import() {
    use wheat_git::finalize_import;
    use wheat_store::{LocalPublicationStore, PublicationError, PublicationStore};
    let git = fixture();
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    let heads = LocalPublicationStore::new(dir.path().join("repository.head"));
    let journal = dir.path().join("import.sqlite");
    let reference = import_repository(git.path(), &content).unwrap();
    stage_import(git.path(), &journal, &objects, &content, Some(1))
        .await
        .unwrap();
    assert!(
        finalize_import(
            git.path(),
            &journal,
            &objects,
            &content,
            &heads,
            "refs/heads/main"
        )
        .await
        .is_err()
    );
    assert!(matches!(heads.read(), Err(PublicationError::Missing)));
    stage_import(git.path(), &journal, &objects, &content, None)
        .await
        .unwrap();
    let head = finalize_import(
        git.path(),
        &journal,
        &objects,
        &content,
        &heads,
        "refs/heads/main",
    )
    .await
    .unwrap();
    assert_eq!(
        finalize_import(
            git.path(),
            &journal,
            &objects,
            &content,
            &heads,
            "refs/heads/main"
        )
        .await
        .unwrap(),
        head
    );
    std::fs::rename(&journal, dir.path().join("inspection.sqlite")).unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    let envelope = reader.envelope(head.envelope).await.unwrap();
    let branches = reader.branch_manifest(envelope.branches).await.unwrap();
    assert_eq!(branches.branches.len(), 2);
    for (name, branch) in reference.branches {
        assert_eq!(
            branches.branches[&format!("refs/heads/{name}")],
            branch.tip.resulting_view
        );
    }
    let state = reader.state(envelope.state).await.unwrap();
    assert_eq!(state.working_view, branches.branches["refs/heads/main"]);
    reader.validate_complete(envelope.state).await.unwrap();
    // Complete-state validation above proves the aggregate contains every historical
    // edit/evidence reference even though individual views contain visible tree content only.
}
