//! Opt-in, bounded, read-only diagnosis of repack lineage eligibility on a local packed store.
#![allow(clippy::items_after_test_module)]

use std::collections::{BTreeMap, BTreeSet};

// A separate thread keeps reporting even if a storage read blocks the test runtime.
struct PhaseProgress {
    sender: Option<std::sync::mpsc::Sender<&'static str>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl PhaseProgress {
    fn start() -> Self {
        use std::sync::mpsc::{self, RecvTimeoutError};
        use std::time::{Duration, Instant};
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            let mut phase = "starting";
            let mut phase_started = started;
            loop {
                match receiver.recv_timeout(Duration::from_secs(5)) {
                    Ok(next) => {
                        phase = next;
                        phase_started = Instant::now();
                        eprintln!("diagnostic +{:?}: {phase}", started.elapsed());
                    }
                    Err(RecvTimeoutError::Timeout) => eprintln!(
                        "diagnostic +{:?}: still {phase} ({:?} in phase)",
                        started.elapsed(),
                        phase_started.elapsed()
                    ),
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            sender: Some(sender),
            worker: Some(worker),
        }
    }

    fn phase(&self, name: &'static str) {
        self.sender.as_ref().unwrap().send(name).unwrap();
    }
}

impl Drop for PhaseProgress {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

/// These are hard failure bounds, not sampling caps. The diagnostic must either
/// inspect every persisted retained view and terminal tree or say precisely why it
/// cannot. Current production observation is 1,797 views and nine terminal trees.
const MAX_RETAINED_VIEWS: usize = 2_000;
const MAX_LINEAGE_EDGES: usize = 10_000;
const MAX_TERMINAL_VIEWS: usize = 32;
const MAX_FILES_PER_TERMINAL: usize = 1_000_000;
const MAX_TOTAL_TERMINAL_FILES: usize = 4_000_000;
const MAX_DIFF_CHANGES_PER_EDGE: usize = 1_000_000;
const MAX_DIFF_NODES_PER_EDGE: usize = 1_000_000;
const MAX_DIFF_ENTRIES_PER_EDGE: usize = 4_000_000;
// A unique graph edge is cursor-walked once. Retaining its bounded diff lets
// shared ancestry update each terminal's active-path map without re-reading it.
const MAX_CACHED_DIFF_CHANGES: usize = 4_000_000;
const MAX_ACTIVE_PATHS: usize = MAX_FILES_PER_TERMINAL;
const MAX_CANDIDATE_IDS: usize = 4_000_000;
const MAX_CANDIDATES_PER_OLDER_ID: usize = 1_024;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_DEPTH: usize = 128;

/// Register an old-to-new content transition. `active` carries the newest
/// content seen for each terminal path while walking backwards through a unique
/// parent chain. Like repack's history planner, it pairs only differing
/// content.
fn register_transition<Id: Copy + Ord>(
    choices: &mut BTreeMap<Id, BTreeSet<Id>>,
    older: Id,
    newer: Id,
) {
    if older != newer {
        choices.entry(older).or_default().insert(newer);
    }
}

/// Small model of the cursor algorithm used by the store diagnostic. It makes
/// the important ordering rule testable without creating a store: each edge is
/// old -> child, and `None` means the path did not exist in the old parent.
fn derive_choices_from_edges<Id: Copy + Ord>(
    terminal: impl IntoIterator<Item = (u8, Id)>,
    edges_newest_to_oldest: impl IntoIterator<Item = Vec<(u8, Option<Id>)>>,
) -> BTreeMap<Id, BTreeSet<Id>> {
    let mut active: BTreeMap<u8, Id> = terminal.into_iter().collect();
    let mut choices = BTreeMap::new();
    for edge in edges_newest_to_oldest {
        for (path, old) in edge {
            let Some(newest) = active.get_mut(&path) else {
                continue;
            };
            match old {
                Some(older) => {
                    register_transition(&mut choices, older, *newest);
                    *newest = older;
                }
                None => {
                    active.remove(&path);
                }
            }
        }
    }
    choices
}

#[test]
fn cursor_derivation_matches_retained_lineage_choices_on_a_small_history() {
    // Path 1's history is [30, 20, 10] (newest first) and path 2's is
    // [40, 30]. Path 3 was created at the tip, so it stops at the first edge.
    let choices = derive_choices_from_edges(
        [(1, 30_u8), (2, 40), (3, 50)],
        [
            vec![(1, Some(20)), (2, Some(30)), (3, None)],
            vec![(1, Some(10)), (2, None)],
        ],
    );
    assert_eq!(
        choices,
        BTreeMap::from([
            (10, BTreeSet::from([20])),
            (20, BTreeSet::from([30])),
            (30, BTreeSet::from([40])),
        ])
    );
}

#[tokio::test]
#[ignore = "set WHEAT_REPACK_DIAGNOSTIC_STORE to an existing local packed store and run explicitly"]
async fn report_repack_lineage_eligibility() {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use wheat_store::{
        LocalPublicationStore, MerkleTreeStore, PackedObjectStore, PublicationStore,
        RepositoryReader, RetainedSetStore, TreeDiffLimits, TreeWalkLimits, ViewLineageMapStore,
    };

    let started = Instant::now();
    let root = PathBuf::from(
        std::env::var("WHEAT_REPACK_DIAGNOSTIC_STORE")
            .expect("set WHEAT_REPACK_DIAGNOSTIC_STORE to a local packed store"),
    );
    let progress = PhaseProgress::start();
    progress.phase("opening store");
    eprintln!("diagnostic store: {}", root.display());
    let objects = PackedObjectStore::open(root.join("storage")).unwrap();
    let content = objects.content_store();
    progress.phase("reading repository head");
    let head = LocalPublicationStore::new(root.join("repository.head"))
        .read()
        .unwrap();
    let reader = RepositoryReader::new(&objects, &content);
    progress.phase("validating envelope");
    let envelope = reader
        .validate_envelope_with_observer(head.envelope, |phase| progress.phase(phase.as_str()))
        .await
        .unwrap();
    if std::env::var_os("WHEAT_REPACK_DIAGNOSTIC_VALIDATE_ONLY").is_some() {
        eprintln!(
            "diagnostic: validated state {} in {:?}",
            envelope.state,
            started.elapsed()
        );
        return;
    }

    progress.phase("reading complete retained view set");
    let state = reader.state(envelope.state).await.unwrap();
    let retained = RetainedSetStore::new(&objects);
    let lineage_store = ViewLineageMapStore::new(&objects);
    let mut retained_cursor = retained.cursor(state.retained_views).await.unwrap();
    let mut view_ids = Vec::with_capacity(1_800);
    while let Some(id) = retained_cursor.next().await.unwrap() {
        assert!(
            view_ids.len() < MAX_RETAINED_VIEWS,
            "retained-view bound {MAX_RETAINED_VIEWS} exceeded; refusing to sample"
        );
        view_ids.push(wheat_core::ViewId::from_digest(*id.as_bytes()));
    }
    eprintln!(
        "diagnostic +{:?}: read all {} retained views",
        started.elapsed(),
        view_ids.len()
    );

    // Persisted lineage is parent -> child-set. Read all its retained edges,
    // rather than filtering a sampled graph.
    progress.phase("reading complete retained view lineage");
    let retained_ids = view_ids.iter().copied().collect::<BTreeSet<_>>();
    let mut parents = BTreeMap::<_, Vec<_>>::new();
    let mut children = BTreeMap::<_, Vec<_>>::new();
    let mut lineage_edges = 0usize;
    for (index, parent) in view_ids.iter().copied().enumerate() {
        if index % 250 == 0 {
            eprintln!(
                "diagnostic +{:?}: lineage parents {index}/{}",
                started.elapsed(),
                view_ids.len()
            );
        }
        let Some(child_root) = lineage_store.get(state.view_lineage, parent).await.unwrap() else {
            continue;
        };
        let mut child_cursor = retained.cursor(child_root).await.unwrap();
        while let Some(child) = child_cursor.next().await.unwrap() {
            assert!(
                lineage_edges < MAX_LINEAGE_EDGES,
                "lineage-edge bound {MAX_LINEAGE_EDGES} exceeded; refusing to sample"
            );
            lineage_edges += 1;
            let child = wheat_core::ViewId::from_digest(*child.as_bytes());
            if retained_ids.contains(&child) {
                parents.entry(child).or_default().push(parent);
                children.entry(parent).or_default().push(child);
            }
        }
    }
    for values in parents.values_mut() {
        values.sort_unstable();
        values.dedup();
    }
    for values in children.values_mut() {
        values.sort_unstable();
        values.dedup();
    }

    progress.phase("reading complete retained view records");
    let mut views = BTreeMap::new();
    for (index, id) in view_ids.iter().copied().enumerate() {
        if index % 250 == 0 {
            eprintln!(
                "diagnostic +{:?}: view records {index}/{}",
                started.elapsed(),
                view_ids.len()
            );
        }
        views.insert(id, reader.view(id).await.unwrap());
    }
    let terminal_ids = view_ids
        .iter()
        .copied()
        .filter(|id| children.get(id).is_none_or(Vec::is_empty))
        .collect::<Vec<_>>();
    assert!(
        terminal_ids.len() <= MAX_TERMINAL_VIEWS,
        "terminal-tree bound {MAX_TERMINAL_VIEWS} exceeded (found {}); refusing to sample",
        terminal_ids.len()
    );
    eprintln!(
        "diagnostic +{:?}: retained views={} retained edges={} terminal trees={} (all included)",
        started.elapsed(),
        views.len(),
        lineage_edges,
        terminal_ids.len()
    );

    let trees = MerkleTreeStore::new(&objects);
    let tree_limits = TreeWalkLimits {
        max_files: MAX_FILES_PER_TERMINAL,
        max_nodes: MAX_FILES_PER_TERMINAL.saturating_mul(4),
        max_entries: MAX_FILES_PER_TERMINAL.saturating_mul(8),
        max_total_path_bytes: 64 * 1024 * 1024,
        max_path_bytes: MAX_PATH_BYTES,
        max_depth: MAX_DEPTH,
        max_node_bytes: 16 * 1024 * 1024,
        max_buffered_entries: MAX_FILES_PER_TERMINAL.min(1_000_000),
    };
    let diff_limits = TreeDiffLimits {
        max_changes: MAX_DIFF_CHANGES_PER_EDGE,
        max_nodes: MAX_DIFF_NODES_PER_EDGE,
        max_entries: MAX_DIFF_ENTRIES_PER_EDGE,
        max_path_bytes: MAX_PATH_BYTES,
        max_depth: MAX_DEPTH,
        max_node_bytes: 16 * 1024 * 1024,
        max_buffered_entries: 1_000_000,
    };

    progress.phase("walking complete terminal trees and unique-parent diffs");
    let mut tips = BTreeSet::new();
    let mut choices = BTreeMap::<_, BTreeSet<_>>::new();
    let mut terminal_files = 0usize;
    let mut unique_parent_edges = 0usize;
    let mut diff_changes = 0usize;
    let mut edge_diffs = BTreeMap::new();
    let mut missing_older_paths = 0usize;
    let mut merge_stops = 0usize;
    let mut root_stops = 0usize;
    let mut last_progress = Instant::now();

    for (terminal_index, terminal_id) in terminal_ids.iter().copied().enumerate() {
        let terminal = &views[&terminal_id];
        // This is the only complete tree walk per terminal. It establishes the
        // active path -> newest-content map used for every ancestor edge.
        let files = trees
            .walk_files(terminal.tree, tree_limits)
            .await
            .unwrap_or_else(|error| {
                panic!("terminal {terminal_id} exceeded tree-walk bounds: {error}")
            });
        assert!(
            files.len() <= MAX_ACTIVE_PATHS,
            "active-path bound {MAX_ACTIVE_PATHS} exceeded; refusing to sample"
        );
        terminal_files = terminal_files
            .checked_add(files.len())
            .expect("file count overflow");
        assert!(
            terminal_files <= MAX_TOTAL_TERMINAL_FILES,
            "total terminal-file bound {MAX_TOTAL_TERMINAL_FILES} exceeded; refusing to sample"
        );
        let mut active = BTreeMap::new();
        for file in files {
            tips.insert(file.value.content);
            active.insert(file.path, file.value.content);
        }

        let mut child_id = terminal_id;
        loop {
            let Some(parent_ids) = parents.get(&child_id) else {
                root_stops += 1;
                break;
            };
            if parent_ids.len() != 1 {
                merge_stops += 1;
                break;
            }
            let parent_id = parent_ids[0];
            let edge = (parent_id, child_id);
            if let std::collections::btree_map::Entry::Vacant(vacant) = edge_diffs.entry(edge) {
                let parent = views.get(&parent_id).unwrap_or_else(|| {
                    panic!("retained lineage parent {parent_id} was not loaded")
                });
                unique_parent_edges += 1;
                let mut cursor = trees.diff_cursor(parent.tree, views[&child_id].tree, diff_limits);
                let mut changes = Vec::new();
                while let Some(change) = cursor.next().await.unwrap_or_else(|error| {
                    panic!("lineage edge {parent_id}->{child_id} exceeded diff bounds: {error}")
                }) {
                    diff_changes += 1;
                    assert!(
                        diff_changes <= MAX_CACHED_DIFF_CHANGES,
                        "cached-diff bound {MAX_CACHED_DIFF_CHANGES} exceeded; refusing to sample"
                    );
                    changes.push(change);
                }
                vacant.insert(changes);
            }
            for change in &edge_diffs[&edge] {
                let Some(newest) = active.get_mut(&change.path) else {
                    continue;
                };
                match change.old {
                    Some(older) => {
                        register_transition(&mut choices, older.content, *newest);
                        assert!(
                            choices.len() <= MAX_CANDIDATE_IDS,
                            "candidate-id bound {MAX_CANDIDATE_IDS} exceeded; refusing to sample"
                        );
                        assert!(
                            choices[&older.content].len() <= MAX_CANDIDATES_PER_OLDER_ID,
                            "per-content candidate bound {MAX_CANDIDATES_PER_OLDER_ID} exceeded for {}",
                            older.content
                        );
                        *newest = older.content;
                    }
                    None => {
                        active.remove(&change.path);
                        missing_older_paths += 1;
                    }
                }
                if last_progress.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "diagnostic +{:?}: terminals={}/{} files={} edges={} diff changes={} active={} candidates={}",
                        started.elapsed(),
                        terminal_index + 1,
                        terminal_ids.len(),
                        terminal_files,
                        unique_parent_edges,
                        diff_changes,
                        active.len(),
                        choices.len()
                    );
                    last_progress = Instant::now();
                }
            }
            child_id = parent_id;
        }
    }

    progress.phase("aggregating exhaustive eligibility categories");
    let unique_newer = choices.values().filter(|newer| newer.len() == 1).count();
    let ambiguous_newer = choices.values().filter(|newer| newer.len() > 1).count();
    let self_transitions = choices
        .iter()
        .filter(|(old, newer)| newer.contains(old))
        .count();
    let forced_full_tip = choices.keys().filter(|id| tips.contains(id)).count();
    let eligible = choices
        .iter()
        .filter(|(old, newer)| !tips.contains(old) && newer.len() == 1 && !newer.contains(old))
        .count();
    let excluded = choices.len() - eligible;
    eprintln!(
        "eligibility exhaustive over {} older IDs: eligible={} excluded={} forced-full-terminal-tip={} ambiguous-newer={} unique-newer={} self-transition={}",
        choices.len(),
        eligible,
        excluded,
        forced_full_tip,
        ambiguous_newer,
        unique_newer,
        self_transitions
    );
    eprintln!(
        "coverage: retained_views={} lineage_edges={} terminal_trees={} terminal_files={} unique_parent_edges={} diff_changes={} missing_older_path_deactivations={} merge_stops={} root_stops={}",
        views.len(),
        lineage_edges,
        terminal_ids.len(),
        terminal_files,
        unique_parent_edges,
        diff_changes,
        missing_older_paths,
        merge_stops,
        root_stops
    );
    eprintln!(
        "manifest comparison skipped: the public packed-store API does not expose the selected content-generation authority or a read-only manifest reader; this diagnostic does not infer authority by scanning generation files."
    );
}
