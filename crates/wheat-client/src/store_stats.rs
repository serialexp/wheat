//! `WHEAT_STORE_STATS=<file>`: what a process read from its Wheat stores, written to
//! the file as JSON when the command ends, so a benchmark can count the work of one
//! command (see `crates/wheat-bench`). Off unless the variable is set. The file is
//! diagnostics output that every process writing it replaces, never read back by Wheat.

use std::{
    path::PathBuf,
    sync::{
        OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use wheat_store::{PackedStoreTrace, PackedStoreTraceSnapshot};

const VARIABLE: &str = "WHEAT_STORE_STATS";

static OBJECT_GETS: AtomicU64 = AtomicU64::new(0);
static OBJECT_GET_BYTES: AtomicU64 = AtomicU64::new(0);
static CONTENT_GETS: AtomicU64 = AtomicU64::new(0);
static CONTENT_GET_BYTES: AtomicU64 = AtomicU64::new(0);
/// Object reads by kind, as `PackedStoreTraceSnapshot::object_gets_by_kind` counts them.
static OBJECT_GETS_BY_KIND: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
const KINDS: usize = wheat_store::ObjectKind::HIGHEST_CODE as usize;

fn path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| std::env::var_os(VARIABLE).map(PathBuf::from))
        .as_ref()
}

/// Whether this process counts what it reads from its stores.
pub fn enabled() -> bool {
    path().is_some()
}

/// Counts one opened store's reads into the process's totals when it is dropped, which
/// must be before the store closes: a field declared before the store's.
#[derive(Debug)]
pub struct Counted(PackedStoreTrace);

impl Counted {
    pub fn new(trace: PackedStoreTrace) -> Self {
        Self(trace)
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        if let Some(snapshot) = self.0.snapshot() {
            add(&snapshot);
        }
    }
}

fn add(snapshot: &PackedStoreTraceSnapshot) {
    OBJECT_GETS.fetch_add(snapshot.object_gets, Ordering::Relaxed);
    OBJECT_GET_BYTES.fetch_add(snapshot.object_get_bytes, Ordering::Relaxed);
    CONTENT_GETS.fetch_add(snapshot.content_gets, Ordering::Relaxed);
    CONTENT_GET_BYTES.fetch_add(snapshot.content_get_bytes, Ordering::Relaxed);
    for (total, gets) in OBJECT_GETS_BY_KIND.iter().zip(snapshot.object_gets_by_kind) {
        total.fetch_add(gets, Ordering::Relaxed);
    }
}

/// Writes the totals to the file `WHEAT_STORE_STATS` names, when it is set.
pub fn write() -> std::io::Result<()> {
    let Some(path) = path() else {
        return Ok(());
    };
    let totals = serde_json::json!({
        "object_gets": OBJECT_GETS.load(Ordering::Relaxed),
        "object_get_bytes": OBJECT_GET_BYTES.load(Ordering::Relaxed),
        "content_gets": CONTENT_GETS.load(Ordering::Relaxed),
        "content_get_bytes": CONTENT_GET_BYTES.load(Ordering::Relaxed),
        "object_gets_by_kind": OBJECT_GETS_BY_KIND
            .iter()
            .map(|gets| gets.load(Ordering::Relaxed))
            .collect::<Vec<_>>(),
    });
    std::fs::write(path, totals.to_string())
}
