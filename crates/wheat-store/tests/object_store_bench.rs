//! Comparative immutable-store benchmark. Run through
//! `scripts/wheat-object-store-bench` to include SeaweedFS.

use std::{collections::BTreeMap, sync::Arc, time::Instant};

use hdrhistogram::Histogram;
use object_store::{ObjectStore as BackendObjectStore, memory::InMemory};
use serde_json::{Value, json};
use wheat_core::*;
use wheat_store::{ContentStore, ImmutableObjectStore, ObjectStoreAdapter, S3ObjectStoreConfig};

fn revision_edits(path: &str, value: &[u8]) -> RevisionEdits {
    let edit = EditRecord::new(
        path,
        EditOperation::ExactFile {
            expected: None,
            replacement: Some(FileValue {
                content: ContentId::of(value),
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
            ..Default::default()
        },
    }
}

const DEFAULT_OBJECTS: usize = 200;
const DEFAULT_LARGE_OBJECTS: usize = 16;
const DEFAULT_LARGE_TREE_ENTRIES: usize = 2_048;
const DEFAULT_BATCH: usize = 64;
const DEFAULT_CONCURRENCY: usize = 16;

struct Corpus {
    objects: Vec<(ObjectId, Arc<Vec<u8>>)>,
}

impl Corpus {
    fn small_revisions(count: usize) -> Self {
        let mut objects = Vec::with_capacity(count);
        let directory = tempfile::tempdir().unwrap();
        let content = ContentStore::new(directory.path());
        for index in 0..count {
            let mut repository = Repository::new(Tree::new());
            let basis = repository.base_view().id;
            let value = format!("value-{index}");
            content.put(value.as_bytes()).unwrap();
            let outcome = repository
                .create_change_with_id(
                    basis,
                    basis,
                    ChangeId::new(),
                    [],
                    revision_edits(&format!("src/{index:06}.rs"), value.as_bytes()),
                    ApplicationContext::new(&content),
                )
                .unwrap();
            push_object(
                &mut objects,
                repository
                    .revision(outcome.revision_id)
                    .unwrap()
                    .canonical_bytes(),
            );
        }
        Self { objects }
    }

    fn large_views(count: usize, entries: usize) -> Self {
        let mut objects = Vec::with_capacity(count);
        for object_index in 0..count {
            let tree = (0..entries)
                .map(|entry| {
                    (
                        format!("src/{object_index:03}/{entry:06}.rs"),
                        wheat_core::FileValue {
                            content: ContentId::of(
                                format!("value-{object_index}-{entry}").as_bytes(),
                            ),
                            kind: wheat_core::FileKind::Regular,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let repository = Repository::new(tree);
            let tree_bytes = repository
                .canonical_closure()
                .objects
                .into_iter()
                .filter(|object| object.bytes.starts_with(&[1, 1]))
                .max_by_key(|object| object.bytes.len())
                .expect("repository closure contains its tree")
                .bytes;
            push_object(&mut objects, tree_bytes);
        }
        Self { objects }
    }
}

fn push_object(objects: &mut Vec<(ObjectId, Arc<Vec<u8>>)>, bytes: Vec<u8>) {
    objects.push((ObjectId::of_canonical_bytes(&bytes), Arc::new(bytes)));
}

struct Metrics {
    histogram: Histogram<u64>,
    operations: usize,
    bytes: usize,
    elapsed_ms: f64,
}

impl Metrics {
    fn new() -> Self {
        Self {
            histogram: Histogram::new(3).unwrap(),
            operations: 0,
            bytes: 0,
            elapsed_ms: 0.0,
        }
    }

    fn record(&mut self, started: Instant, bytes: usize) {
        self.histogram
            .record(started.elapsed().as_micros().max(1) as u64)
            .unwrap();
        self.operations += 1;
        self.bytes += bytes;
    }

    fn finish(mut self, started: Instant) -> Self {
        self.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self
    }

    fn json(&self, backend: &str, workload: &str) -> Value {
        json!({
            "backend": backend,
            "workload": workload,
            "operations": self.operations,
            "bytes": self.bytes,
            "elapsed_ms": self.elapsed_ms,
            "ops_per_second": self.operations as f64 / (self.elapsed_ms / 1000.0),
            "mib_per_second": self.bytes as f64 / (1024.0 * 1024.0) / (self.elapsed_ms / 1000.0),
            "latency_us": {
                "p50": self.histogram.value_at_quantile(0.50),
                "p95": self.histogram.value_at_quantile(0.95),
                "p99": self.histogram.value_at_quantile(0.99),
                "max": self.histogram.max(),
            }
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "benchmark; run via scripts/wheat-object-store-bench"]
async fn compare_memory_and_configured_object_store() {
    let object_count = env_usize("WHEAT_BENCH_OBJECTS", DEFAULT_OBJECTS);
    let large_object_count = env_usize("WHEAT_BENCH_LARGE_OBJECTS", DEFAULT_LARGE_OBJECTS);
    let large_tree_entries =
        env_usize("WHEAT_BENCH_LARGE_TREE_ENTRIES", DEFAULT_LARGE_TREE_ENTRIES);
    let batch_size = env_usize("WHEAT_BENCH_BATCH", DEFAULT_BATCH).min(object_count);
    let concurrency = env_usize("WHEAT_BENCH_CONCURRENCY", DEFAULT_CONCURRENCY).max(1);
    let corpora = [
        (
            "small_revision",
            Arc::new(Corpus::small_revisions(object_count)),
        ),
        (
            "large_view",
            Arc::new(Corpus::large_views(large_object_count, large_tree_entries)),
        ),
    ];

    let mut results = Vec::new();
    for (corpus_name, corpus) in corpora {
        let memory: Arc<dyn BackendObjectStore> = Arc::new(InMemory::new());
        let memory = Arc::new(ObjectStoreAdapter::new(
            memory,
            format!("wheat-bench/memory/{corpus_name}"),
        ));
        results.extend(
            run_backend(
                "memory",
                corpus_name,
                memory,
                Arc::clone(&corpus),
                batch_size.min(corpus.objects.len()),
                concurrency,
            )
            .await,
        );

        if let Some(mut config) = s3_config() {
            config.prefix = format!("{}/{corpus_name}", config.prefix);
            let remote = Arc::new(ObjectStoreAdapter::s3(&config).unwrap());
            results.extend(
                run_backend(
                    "object_store",
                    corpus_name,
                    remote,
                    corpus,
                    batch_size.min(large_object_count.max(object_count)),
                    concurrency,
                )
                .await,
            );
        }
    }

    let report = json!({
        "format": "wheat-object-store-benchmark-v1",
        "configuration": {
            "small_objects": object_count,
            "large_objects": large_object_count,
            "large_tree_entries": large_tree_entries,
            "batch": batch_size,
            "concurrency": concurrency,
        },
        "results": results,
    });
    let rendered = serde_json::to_string_pretty(&report).unwrap();
    println!("{rendered}");
    if let Ok(path) = std::env::var("WHEAT_BENCH_OUTPUT") {
        std::fs::write(path, format!("{rendered}\n")).unwrap();
    }
}

async fn run_backend<S>(
    name: &str,
    corpus_name: &str,
    store: Arc<S>,
    corpus: Arc<Corpus>,
    batch_size: usize,
    concurrency: usize,
) -> Vec<Value>
where
    S: ImmutableObjectStore + 'static,
{
    let cold = concurrent_puts(Arc::clone(&store), Arc::clone(&corpus), concurrency).await;
    let dedup = concurrent_puts(Arc::clone(&store), Arc::clone(&corpus), concurrency).await;
    let reads = concurrent_reads(Arc::clone(&store), Arc::clone(&corpus), concurrency).await;

    let ids = corpus
        .objects
        .iter()
        .take(batch_size)
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    let started = Instant::now();
    let exists = store.exists_many(&ids).await.unwrap();
    assert!(exists.into_iter().all(|exists| exists));
    let mut batch = Metrics::new();
    batch.record(started, 0);
    batch = batch.finish(started);

    vec![
        cold.json(name, &format!("{corpus_name}_cold_put")),
        dedup.json(name, &format!("{corpus_name}_deduplicated_put")),
        reads.json(name, &format!("{corpus_name}_verified_get")),
        batch.json(name, &format!("{corpus_name}_exists_many_batch")),
    ]
}

async fn concurrent_puts<S>(store: Arc<S>, corpus: Arc<Corpus>, concurrency: usize) -> Metrics
where
    S: ImmutableObjectStore + 'static,
{
    let started_all = Instant::now();
    let mut metrics = Metrics::new();
    for chunk in corpus.objects.chunks(concurrency) {
        let tasks = chunk
            .iter()
            .map(|(id, bytes)| {
                let store = Arc::clone(&store);
                let bytes = Arc::clone(bytes);
                let id = *id;
                tokio::spawn(async move {
                    let started = Instant::now();
                    store.put_if_absent(id, &bytes).await.unwrap();
                    (started, bytes.len())
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            let (started, bytes) = task.await.unwrap();
            metrics.record(started, bytes);
        }
    }
    metrics.finish(started_all)
}

async fn concurrent_reads<S>(store: Arc<S>, corpus: Arc<Corpus>, concurrency: usize) -> Metrics
where
    S: ImmutableObjectStore + 'static,
{
    let started_all = Instant::now();
    let mut metrics = Metrics::new();
    for chunk in corpus.objects.chunks(concurrency) {
        let tasks = chunk
            .iter()
            .map(|(id, _)| {
                let store = Arc::clone(&store);
                let id = *id;
                tokio::spawn(async move {
                    let started = Instant::now();
                    let object = store.get(id).await.unwrap();
                    (started, object.canonical_bytes.len())
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            let (started, bytes) = task.await.unwrap();
            metrics.record(started, bytes);
        }
    }
    metrics.finish(started_all)
}

fn s3_config() -> Option<S3ObjectStoreConfig> {
    let endpoint = std::env::var("WHEAT_TEST_S3_ENDPOINT").ok()?;
    Some(S3ObjectStoreConfig {
        endpoint,
        bucket: std::env::var("WHEAT_TEST_S3_BUCKET").ok()?,
        region: std::env::var("WHEAT_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key_id: std::env::var("WHEAT_TEST_S3_ACCESS_KEY_ID").ok()?,
        secret_access_key: std::env::var("WHEAT_TEST_S3_SECRET_ACCESS_KEY").ok()?,
        prefix: format!("wheat-bench/{}", std::process::id()),
        allow_http: true,
    })
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
