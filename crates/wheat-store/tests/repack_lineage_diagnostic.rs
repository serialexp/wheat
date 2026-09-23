//! Opt-in read-only diagnosis of repack lineage eligibility on a real local packed store.
#![allow(clippy::items_after_test_module)]

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

#[tokio::test]
#[ignore = "set WHEAT_REPACK_DIAGNOSTIC_STORE to an existing local packed store and run explicitly"]
async fn report_repack_lineage_eligibility() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use wheat_store::{
        LocalPublicationStore, PackedObjectStore, PublicationStore, RepositoryReader,
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
    progress.phase("validation complete");
    if std::env::var_os("WHEAT_REPACK_DIAGNOSTIC_VALIDATE_ONLY").is_some() {
        eprintln!(
            "diagnostic: validated state {} in {:?}",
            envelope.state,
            started.elapsed()
        );
        return;
    }
    progress.phase("hydrating repository");
    let repository = reader.hydrate(envelope.state).await.unwrap();
    progress.phase("collecting retained views");
    let views: BTreeMap<_, _> = repository.retained_views().map(|v| (v.id, v)).collect();
    progress.phase("collecting view lineage");
    let mut parents = BTreeMap::<_, Vec<_>>::new();
    let mut children = BTreeMap::<_, Vec<_>>::new();
    for (parent, child) in repository.view_lineage() {
        parents.entry(child).or_default().push(parent);
        children.entry(parent).or_default().push(child);
    }
    progress.phase("traversing terminal view paths");
    eprintln!(
        "diagnostic: {} views, {} lineage edges",
        views.len(),
        parents.values().map(Vec::len).sum::<usize>()
    );
    let mut last_progress = Instant::now();
    let mut visited_views = 0usize;
    let mut tips = BTreeSet::new();
    let mut choices = BTreeMap::<_, BTreeSet<_>>::new();
    let mut lineages = 0usize;
    let mut repeated = 0usize;
    let mut total_edges = 0usize;
    let mut long_lineages = 0usize;
    let mut longest = 0usize;
    let mut stopped_on_missing_path = 0usize;
    let mut stopped_on_parent_ambiguity = 0usize;
    let mut stopped_on_missing_parent_view = 0usize;
    let mut length_histogram = [0usize; 12];
    for (tip_id, tip) in &views {
        visited_views += 1;
        if last_progress.elapsed() >= Duration::from_secs(5) {
            eprintln!(
                "diagnostic +{:?}: traversal {visited_views}/{} views, {lineages} paths, {total_edges} edges",
                started.elapsed(),
                views.len()
            );
            last_progress = Instant::now();
        }
        if children.get(tip_id).is_some_and(|c| !c.is_empty()) {
            continue;
        }
        for path in tip.tree.keys() {
            lineages += 1;
            let mut lineage = Vec::new();
            let mut cursor = *tip_id;
            loop {
                if last_progress.elapsed() >= Duration::from_secs(5) {
                    eprintln!(
                        "diagnostic +{:?}: traversal {visited_views}/{} views, {lineages} paths, {total_edges} edges; current path depth {}",
                        started.elapsed(),
                        views.len(),
                        lineage.len()
                    );
                    last_progress = Instant::now();
                }
                let Some(view) = views.get(&cursor) else {
                    stopped_on_missing_parent_view += 1;
                    break;
                };
                let Some(id) = view.tree.get(path).map(|f| f.content) else {
                    stopped_on_missing_path += 1;
                    break;
                };
                if lineage.last() == Some(&id) {
                    repeated += 1;
                } else {
                    lineage.push(id);
                }
                let Some(parent) = parents.get(&cursor).filter(|p| p.len() == 1) else {
                    stopped_on_parent_ambiguity += 1;
                    break;
                };
                cursor = parent[0];
            }
            if let Some(&tip) = lineage.first() {
                tips.insert(tip);
            }
            longest = longest.max(lineage.len());
            if lineage.len() >= 16 {
                long_lineages += 1;
            }
            let bin = lineage.len().min(11);
            length_histogram[bin] += 1;
            for pair in lineage.windows(2) {
                choices.entry(pair[1]).or_default().insert(pair[0]);
                total_edges += 1;
            }
        }
    }
    progress.phase("aggregating lineage eligibility");
    let distinct_targets = choices.len();
    let conflicts = choices
        .iter()
        .filter(|(id, bases)| bases.len() != 1 || bases.contains(id))
        .count();
    let candidate_targets = choices
        .iter()
        .filter(|(id, bases)| !tips.contains(id) && bases.len() == 1 && !bases.contains(id))
        .count();
    eprintln!(
        "retained views={} terminal views={} lineages={} max_length={} >=16={} repeated_same_content={}",
        views.len(),
        views
            .keys()
            .filter(|id| children.get(*id).is_none_or(Vec::is_empty))
            .count(),
        lineages,
        longest,
        long_lineages,
        repeated
    );
    eprintln!("length histogram [0..10,11+]={length_histogram:?}");
    eprintln!(
        "lineage edges={} distinct older target IDs={} forced-full tip IDs={} conflicting target IDs={} candidate target IDs={}",
        total_edges,
        distinct_targets,
        tips.len(),
        conflicts,
        candidate_targets
    );
    eprintln!(
        "stops missing_path={} ambiguous_parent={} missing_parent_view={}",
        stopped_on_missing_path, stopped_on_parent_ambiguity, stopped_on_missing_parent_view
    );
}
