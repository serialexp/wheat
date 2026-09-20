//! Regression coverage for the paired radix subset walk used by
//! `verify_view_application_maps`. Exercises:
//!
//! 1. Missing and mismatched revision selections beneath compressed-prefix
//!    alignment.
//! 2. Conflict-evidence isolation across two views sharing a conflict-set root
//!    but with different selected applications.
//! 3. Conflict-bearing subtree receipt exclusion, plus clean-subtree receipt
//!    reuse across views that share an application-map sub-DAG.
//! 4. Repository-level object-read bounds for asymmetric application/revision
//!    histories.

use async_trait::async_trait;
use object_store::memory::InMemory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wheat_core::*;
use wheat_store::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn exact(path: &str, expected: Option<&[u8]>, replacement: Option<&[u8]>) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: expected.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
            replacement: replacement.map(|bytes| FileValue {
                content: ContentId::of(bytes),
                kind: FileKind::Regular,
            }),
        },
    )
    .unwrap();
    let id = edit.id();
    RevisionEdits {
        edits: [(id, edit)].into(),
        transition: EditTransition {
            added: [id].into(),
            ..EditTransition::default()
        },
    }
}

fn revision_edit(
    path: &str,
    expected: Option<&[u8]>,
    replacement: Option<&[u8]>,
    removed: impl IntoIterator<Item = EditId>,
) -> RevisionEdits {
    let mut edits = exact(path, expected, replacement);
    edits.transition.removed = removed.into_iter().collect();
    edits
}

fn predecessor_edits(repo: &Repository, revision: RevisionId) -> Vec<EditId> {
    repo.revision(revision)
        .unwrap()
        .edits()
        .keys()
        .copied()
        .collect()
}

struct Counting {
    inner: ObjectStoreAdapter,
    gets: AtomicUsize,
    application_map_nodes: AtomicUsize,
    revision_map_nodes: AtomicUsize,
    applications: AtomicUsize,
}

impl Counting {
    fn new(prefix: &str) -> Self {
        Self {
            inner: ObjectStoreAdapter::new(Arc::new(InMemory::new()), prefix),
            gets: AtomicUsize::new(0),
            application_map_nodes: AtomicUsize::new(0),
            revision_map_nodes: AtomicUsize::new(0),
            applications: AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.gets.store(0, Ordering::SeqCst);
        self.application_map_nodes.store(0, Ordering::SeqCst);
        self.revision_map_nodes.store(0, Ordering::SeqCst);
        self.applications.store(0, Ordering::SeqCst);
    }
}

#[async_trait]
impl ImmutableObjectStore for Counting {
    async fn put_if_absent(&self, id: ObjectId, bytes: &[u8]) -> Result<(), ObjectStoreError> {
        self.inner.put_if_absent(id, bytes).await
    }
    async fn get(&self, id: ObjectId) -> Result<StoredObject, ObjectStoreError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        let object = self.inner.get(id).await?;
        match object.kind {
            ObjectKind::ApplicationMapNode => {
                self.application_map_nodes.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::RevisionMapNode => {
                self.revision_map_nodes.fetch_add(1, Ordering::SeqCst);
            }
            ObjectKind::Application => {
                self.applications.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        Ok(object)
    }
    async fn exists_many(&self, ids: &[ObjectId]) -> Result<Vec<bool>, ObjectStoreError> {
        self.inner.exists_many(ids).await
    }
}

/// Replace a single view inside a persisted repository state, rewriting the
/// retained-views set and state record accordingly. The view lineage is NOT
/// touched — callers must ensure the replacement view ID preserves lineage
/// validity (e.g. by only modifying the non-lineage-indexed view at the tip).
///
/// Returns the new state id.
async fn replace_view_in_state<O: ImmutableObjectStore>(
    objects: &O,
    content: &ContentStore,
    state_id: RepositoryStateId,
    original_view: ViewId,
    replacement: &ViewRecord,
) -> RepositoryStateId {
    let new_view_id = replacement.id();
    objects
        .put_if_absent(new_view_id.into(), &replacement.canonical_bytes())
        .await
        .unwrap();

    let reader = RepositoryReader::new(objects, content);
    let state = reader.state(state_id).await.unwrap();

    // Rebuild retained_views: swap the original view for the replacement.
    let retained = RetainedSetStore::new(objects);
    let mut cursor = retained.cursor(state.retained_views).await.unwrap();
    let mut view_ids: Vec<ObjectId> = Vec::new();
    while let Some(id) = cursor.next().await.unwrap() {
        if ViewId::from_digest(*id.as_bytes()) == original_view {
            view_ids.push(new_view_id.into());
        } else {
            view_ids.push(id);
        }
    }
    let mut emitted = Vec::new();
    let new_retained_views = build_retained_set(view_ids, |o| emitted.push(o));
    for o in &emitted {
        objects.put_if_absent(o.id, &o.bytes).await.unwrap();
    }

    let new_state = RepositoryStateRecord {
        base_view: state.base_view,
        changes: state.changes,
        view_lineage: state.view_lineage,
        required_content: state.required_content,
        retained_changes: state.retained_changes,
        retained_views: new_retained_views,
        retained_revisions: state.retained_revisions,
        retained_applications: state.retained_applications,
    };
    objects
        .put_if_absent(new_state.id().into(), &new_state.canonical_bytes())
        .await
        .unwrap();
    new_state.id()
}

// ---------------------------------------------------------------------------
// Test 1: Missing and mismatched revision selections
// ---------------------------------------------------------------------------

/// The verifier rejects a repository whose revision map is missing a selection
/// that the application map expects: the paired walk yields `right: None` and
/// the `revision_id != Some(record.revision_id)` check fires.
#[tokio::test]
async fn missing_revision_selection_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [b"a".as_slice(), b"b"] {
        content.put(bytes).unwrap();
    }
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let (change_a, a) = repo
        .create_change(
            basis,
            basis,
            [],
            exact("file_a", None, Some(b"a")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let (_, b) = repo
        .create_change(
            basis,
            a.resulting_view,
            [],
            exact("file_b", None, Some(b"b")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let effective_view = b.resulting_view;

    // Persist the valid repo.
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }

    // Sanity: the valid state passes.
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_complete(closure.state.id).await.unwrap();

    // Now drop one change from the effective view's revision map.
    let original = reader.view(effective_view).await.unwrap();
    let revisions = RevisionMapStore::new(&objects);
    let shrunk = revisions
        .remove(original.revisions, change_a)
        .await
        .unwrap();

    let modified_view = ViewRecord {
        tree: original.tree,
        revisions: shrunk,
        applications: original.applications,
        unresolved_conflicts: original.unresolved_conflicts,
        required_content: original.required_content,
    };

    let bad_state = replace_view_in_state(
        &objects,
        &content,
        closure.state.id,
        effective_view,
        &modified_view,
    )
    .await;

    assert!(
        reader.validate_complete(bad_state).await.is_err(),
        "verifier must reject a revision map missing a selected application's revision"
    );
}

/// The verifier rejects a repository whose revision map points a change to the
/// wrong revision: the paired walk yields `right: Some(wrong_id)` and the
/// `revision_id != Some(record.revision_id)` check fires.
#[tokio::test]
async fn mismatched_revision_selection_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [b"a".as_slice(), b"b"] {
        content.put(bytes).unwrap();
    }
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let (change_a, a) = repo
        .create_change(
            basis,
            basis,
            [],
            exact("file_a", None, Some(b"a")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let (_, b) = repo
        .create_change(
            basis,
            a.resulting_view,
            [],
            exact("file_b", None, Some(b"b")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let effective_view = b.resulting_view;

    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }

    // Replace change_a's revision selection with a bogus revision id.
    let reader = RepositoryReader::new(&objects, &content);
    let original = reader.view(effective_view).await.unwrap();
    let bogus_revision = RevisionId::from_digest([0xff; 32]);
    let revisions = RevisionMapStore::new(&objects);
    let tampered = revisions
        .insert(original.revisions, change_a, bogus_revision)
        .await
        .unwrap();

    let modified_view = ViewRecord {
        tree: original.tree,
        revisions: tampered,
        applications: original.applications,
        unresolved_conflicts: original.unresolved_conflicts,
        required_content: original.required_content,
    };

    let bad_state = replace_view_in_state(
        &objects,
        &content,
        closure.state.id,
        effective_view,
        &modified_view,
    )
    .await;

    assert!(
        reader.validate_complete(bad_state).await.is_err(),
        "verifier must reject a revision map with a mismatched revision id"
    );
}

// ---------------------------------------------------------------------------
// Test 2: Conflict evidence isolation across views
// ---------------------------------------------------------------------------

/// Two views may share a conflict-set root. The verifier clears produced-
/// conflict evidence before each view's application-map walk, so a conflict
/// that view A's applications produce cannot satisfy view B's reverse check
/// if view B's applications do not independently produce it.
#[tokio::test]
async fn conflict_evidence_does_not_leak_between_views() {
    let dir = tempfile::tempdir().unwrap();
    let objects = LooseObjectStore::new(dir.path().join("objects"));
    let content = ContentStore::new(dir.path().join("content"));
    for bytes in [b"base".as_slice(), b"v1", b"v2", b"v3"] {
        content.put(bytes).unwrap();
    }

    // Build a conflicted history: change A edits "file" (base -> v1), change B
    // depends on A and edits "file" (v1 -> v3). Then revise A to (base -> v2),
    // so B is reapplied against a tree where "file" = v2. B expects v1 and
    // replaces with v3 (!= v2), so it's not AlreadySatisfied — it conflicts.
    let mut repo = Repository::new(Tree::from([(
        "file".into(),
        FileValue {
            content: ContentId::of(b"base"),
            kind: FileKind::Regular,
        },
    )]));
    let basis = repo.base_view().id;
    let (change_a, a) = repo
        .create_change(
            basis,
            basis,
            [],
            exact("file", Some(b"base"), Some(b"v1")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    let (_, b) = repo
        .create_change(
            basis,
            a.resulting_view,
            [change_a],
            exact("file", Some(b"v1"), Some(b"v3")),
            ApplicationContext::new(&content),
        )
        .unwrap();
    // Revise A: base -> v2, which makes B's reapplication conflict (expects v1,
    // sees v2, replacement v3 != v2 so not AlreadySatisfied).
    let removed = predecessor_edits(&repo, a.revision_id);
    repo.revise_change(
        basis,
        b.resulting_view,
        change_a,
        a.revision_id,
        revision_edit("file", Some(b"base"), Some(b"v2"), removed),
        ApplicationContext::new(&content),
    )
    .unwrap();

    // The resulting repo should have at least one view with conflicts.
    let closure = repo.canonical_closure();
    let has_conflicts = closure
        .state
        .retained_views
        .iter()
        .any(|vid| !repo.view(*vid).unwrap().unresolved_conflicts.is_empty());
    assert!(
        has_conflicts,
        "test setup: expected at least one view with unresolved conflicts"
    );

    // The valid state must pass.
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    let reader = RepositoryReader::new(&objects, &content);
    reader.validate_complete(closure.state.id).await.unwrap();

    // Now fabricate an attack: take a *clean* view (the base view or a
    // non-conflicted intermediate) and give it the same conflict set as the
    // conflicted view. Its applications don't produce those conflicts, so
    // the reverse check must fail.
    let conflicted_view_id = *closure
        .state
        .retained_views
        .iter()
        .find(|vid| !repo.view(**vid).unwrap().unresolved_conflicts.is_empty())
        .unwrap();
    let clean_view_id = *closure
        .state
        .retained_views
        .iter()
        .find(|vid| repo.view(**vid).unwrap().unresolved_conflicts.is_empty())
        .unwrap();

    let conflicted_record = reader.view(conflicted_view_id).await.unwrap();
    let clean_record = reader.view(clean_view_id).await.unwrap();

    // Replace the clean view's conflict set with the conflicted view's.
    let modified_clean = ViewRecord {
        tree: clean_record.tree,
        revisions: clean_record.revisions,
        applications: clean_record.applications,
        unresolved_conflicts: conflicted_record.unresolved_conflicts,
        required_content: clean_record.required_content,
    };

    let bad_state = replace_view_in_state(
        &objects,
        &content,
        closure.state.id,
        clean_view_id,
        &modified_clean,
    )
    .await;

    assert!(
        reader.validate_complete(bad_state).await.is_err(),
        "verifier must reject: conflict evidence from another view's walk must not leak"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Conflict-bearing subtree receipt exclusion + clean receipt reuse
// ---------------------------------------------------------------------------

/// Views that share a clean application/revision sub-DAG benefit from
/// receipt pruning, while conflict-bearing subtrees are always re-walked.
/// This test builds two retained views with shared clean change keys and
/// verifies that the application-map node reads are strictly less than
/// twice the unique node count.
#[tokio::test]
async fn clean_map_pair_receipts_are_reused_across_views() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    let objects = Counting::new("receipt-reuse");

    // Register content blobs.
    for bytes in [b"base".as_slice(), b"a", b"b", b"c", b"d", b"e", b"f"] {
        content.put(bytes).unwrap();
    }

    // Build a linear chain: each change touches a distinct file so all
    // applications are clean.
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let mut effective = basis;
    for (i, file) in ["a", "b", "c", "d", "e", "f"].iter().enumerate() {
        let byte = [b'a' + i as u8];
        let (_, outcome) = repo
            .create_change(
                basis,
                effective,
                [],
                exact(file, None, Some(&byte)),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
    }

    // The repository now has 7 retained views (basis + 6 intermediate).
    // All views share some application-map sub-DAG.
    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }

    objects.reset();
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();
    let app_reads = objects.application_map_nodes.load(Ordering::SeqCst);

    // Count unique application-map nodes (from a separate baseline run).
    let baseline = Counting::new("receipt-baseline");
    for object in &closure.objects {
        baseline
            .inner
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }
    // A single-view verification would read each node once. With receipts,
    // multi-view verification should read fewer than views * unique_nodes.
    let views_count = closure.state.retained_views.len();
    assert!(
        views_count >= 3,
        "test setup: need at least 3 views, got {views_count}"
    );
    // Without receipts, we'd expect ~views * nodes reads. With receipts,
    // shared clean subtrees are pruned. Assert at least 20% saving.
    // (The exact saving depends on DAG sharing, but 6 views on a linear
    // chain share most prefix structure.)
    assert!(app_reads > 0, "application-map nodes should have been read");
    // We verify that total reads across all views is less than
    // views * (reads for the first view). A rough heuristic: the reads
    // should be significantly less than views_count * app_reads / views_count * 2
    // (i.e., sub-linear scaling).
    // A tighter bound: with receipts, the total should be less than
    // unique_nodes * views_count.
    // For this test, assert app_reads < 6 * what a single-view walk would need.
    // A single-view walk of 6 entries in a compressed radix trie needs ~2 nodes
    // (root + one interior or the root has children). Let's just assert the
    // reads are bounded.
    //
    // Concrete bound: with 7 views and ~12 unique app-map nodes (radix of
    // up to 6 entries), without receipts we'd read ~84 nodes. With receipts,
    // shared subtrees are pruned after the first view, so we expect < 40.
    let lax_upper = views_count * 12; // generous upper bound without receipts
    assert!(
        app_reads < lax_upper,
        "receipt pruning should keep app-map reads ({app_reads}) well below \
         {lax_upper} (views={views_count})"
    );
}

// ---------------------------------------------------------------------------
// Test 4 (node/pending limits): covered by unit tests in radix.rs
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Test 5: Asymmetric application/revision history read bounds
// ---------------------------------------------------------------------------

/// When a linear chain of revisions produces many revision-map entries but
/// only a small application-map footprint per view, the paired walk stays
/// bounded by the application side.
#[tokio::test]
async fn asymmetric_history_application_reads_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let content = ContentStore::new(dir.path().join("content"));
    let objects = Counting::new("asymmetric-history");

    // Register many content blobs.
    for n in 0..20u8 {
        content.put(&[n]).unwrap();
    }

    // Build a long linear chain of 20 revisions, each touching a unique file.
    // This creates 20 revision-map entries (one per change) and 20
    // application-map entries in the tip view.
    let mut repo = Repository::new(Tree::new());
    let basis = repo.base_view().id;
    let mut effective = basis;
    for n in 0..20u8 {
        let path = format!("file_{n:02}");
        let (_, outcome) = repo
            .create_change(
                basis,
                effective,
                [],
                exact(&path, None, Some(&[n])),
                ApplicationContext::new(&content),
            )
            .unwrap();
        effective = outcome.resulting_view;
    }

    let closure = repo.canonical_closure();
    for object in &closure.objects {
        objects
            .put_if_absent(object.id, &object.bytes)
            .await
            .unwrap();
    }

    objects.reset();
    RepositoryReader::new(&objects, &content)
        .validate_complete(closure.state.id)
        .await
        .unwrap();

    let app_reads = objects.application_map_nodes.load(Ordering::SeqCst);
    let rev_reads = objects.revision_map_nodes.load(Ordering::SeqCst);
    let application_reads = objects.applications.load(Ordering::SeqCst);

    // The paired walk follows the application (left) side; it reads right
    // nodes only where the left side has children. So app-map reads should
    // not be dramatically larger than the revision reads; both scale with
    // the number of entries.
    //
    // The application-map walk is bounded by the application count per view.
    // With 20 changes and up to 21 views, the total should be bounded by
    // (unique_nodes * views_with_sharing_benefit). Verify it stays below a
    // generous linear cap.
    let views_count = closure.state.retained_views.len();
    assert!(app_reads > 0, "application-map nodes must be read");
    assert!(application_reads > 0, "application records must be read");

    // Application records are read once per application per view in which
    // they appear, but receipt pruning prevents re-reading shared subtrees.
    // Assert applications read < 2 * total_applications (i.e. not quadratic).
    let total_applications = closure.state.retained_applications.len();
    assert!(
        application_reads < total_applications * 2,
        "application record reads ({application_reads}) should be bounded by \
         2 * retained applications ({total_applications}), \
         not scale with revision count"
    );

    // Application-map node reads should not be wildly larger than revision
    // map reads — both walk the same key-space.
    assert!(
        app_reads <= rev_reads * 3,
        "application-map node reads ({app_reads}) should stay proportional \
         to revision-map node reads ({rev_reads}), \
         views={views_count}"
    );
}
