use std::{
    sync::{
        Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

const OBJECT_KIND_COUNT: usize = 18;

/// Cumulative packed-store activity captured with bounded, constant-memory counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PackedStoreTraceSnapshot {
    pub object_gets: u64,
    pub object_get_bytes: u64,
    pub object_put_batches: u64,
    pub object_put_attempts: u64,
    pub object_put_bytes: u64,
    pub object_gets_by_kind: [u64; OBJECT_KIND_COUNT],
    pub content_gets: u64,
    pub content_get_bytes: u64,
    pub content_put_batches: u64,
    pub content_put_attempts: u64,
    pub content_put_bytes: u64,
    pub catalog_gets: u64,
    pub catalog_bulk_hits: u64,
    pub catalog_locator_hits: u64,
    pub deferred_put_calls: u64,
    pub deferred_put_attempts: u64,
    pub deferred_put_duplicates: u64,
    pub deferred_appends: u64,
    pub deferred_index_transactions: u64,
    pub locator_recounts: u64,
    pub bulk_rolls: u64,
    pub bulk_finalizations: u64,
    pub catalog_lookup_ns: u64,
    pub deferred_append_ns: u64,
    pub deferred_index_transaction_ns: u64,
    pub locator_recount_ns: u64,
    pub bulk_finalization_ns: u64,
}

/// Read handle for diagnostics enabled on a packed store.
///
/// This handle does not keep the store open. `snapshot` returns `None` after the
/// packed engine has been closed.
#[derive(Clone, Debug)]
pub struct PackedStoreTrace {
    pub(crate) counters: Weak<PackedStoreTraceCounters>,
}

impl PackedStoreTrace {
    #[must_use]
    pub fn snapshot(&self) -> Option<PackedStoreTraceSnapshot> {
        self.counters.upgrade().map(|counters| counters.snapshot())
    }
}

#[derive(Debug, Default)]
pub(crate) struct PackedStoreTraceCounters {
    object_gets: AtomicU64,
    object_get_bytes: AtomicU64,
    object_put_batches: AtomicU64,
    object_put_attempts: AtomicU64,
    object_put_bytes: AtomicU64,
    object_gets_by_kind: [AtomicU64; OBJECT_KIND_COUNT],
    content_gets: AtomicU64,
    content_get_bytes: AtomicU64,
    content_put_batches: AtomicU64,
    content_put_attempts: AtomicU64,
    content_put_bytes: AtomicU64,
    catalog_gets: AtomicU64,
    catalog_bulk_hits: AtomicU64,
    catalog_locator_hits: AtomicU64,
    deferred_put_calls: AtomicU64,
    deferred_put_attempts: AtomicU64,
    deferred_put_duplicates: AtomicU64,
    deferred_appends: AtomicU64,
    deferred_index_transactions: AtomicU64,
    locator_recounts: AtomicU64,
    bulk_rolls: AtomicU64,
    bulk_finalizations: AtomicU64,
    catalog_lookup_ns: AtomicU64,
    deferred_append_ns: AtomicU64,
    deferred_index_transaction_ns: AtomicU64,
    locator_recount_ns: AtomicU64,
    bulk_finalization_ns: AtomicU64,
}

impl PackedStoreTraceCounters {
    fn add(counter: &AtomicU64, value: usize) {
        counter.fetch_add(value as u64, Ordering::Relaxed);
    }

    fn add_duration(counter: &AtomicU64, value: Duration) {
        counter.fetch_add(
            u64::try_from(value.as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn object_get(&self, kind: u8, bytes: usize) {
        self.object_gets.fetch_add(1, Ordering::Relaxed);
        Self::add(&self.object_get_bytes, bytes);
        self.object_gets_by_kind[usize::from(kind) - 1].fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn object_put_batch(&self, attempts: usize, bytes: usize) {
        self.object_put_batches.fetch_add(1, Ordering::Relaxed);
        Self::add(&self.object_put_attempts, attempts);
        Self::add(&self.object_put_bytes, bytes);
    }

    pub(crate) fn content_get(&self, bytes: usize) {
        self.content_gets.fetch_add(1, Ordering::Relaxed);
        Self::add(&self.content_get_bytes, bytes);
    }

    pub(crate) fn content_put_batch(&self, attempts: usize, bytes: usize) {
        self.content_put_batches.fetch_add(1, Ordering::Relaxed);
        Self::add(&self.content_put_attempts, attempts);
        Self::add(&self.content_put_bytes, bytes);
    }

    pub(crate) fn catalog_get(&self) {
        self.catalog_gets.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn catalog_bulk_hit(&self) {
        self.catalog_bulk_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn catalog_locator_hit(&self) {
        self.catalog_locator_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn deferred_put(&self, attempts: usize) {
        self.deferred_put_calls.fetch_add(1, Ordering::Relaxed);
        Self::add(&self.deferred_put_attempts, attempts);
    }

    pub(crate) fn deferred_duplicate(&self) {
        self.deferred_put_duplicates.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn deferred_append(&self) {
        self.deferred_appends.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn deferred_index_transaction(&self) {
        self.deferred_index_transactions
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn locator_recount(&self) {
        self.locator_recounts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn bulk_roll(&self) {
        self.bulk_rolls.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn bulk_finalization(&self) {
        self.bulk_finalizations.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn catalog_lookup_duration(&self, duration: Duration) {
        Self::add_duration(&self.catalog_lookup_ns, duration);
    }

    pub(crate) fn deferred_append_duration(&self, duration: Duration) {
        Self::add_duration(&self.deferred_append_ns, duration);
    }

    pub(crate) fn deferred_index_transaction_duration(&self, duration: Duration) {
        Self::add_duration(&self.deferred_index_transaction_ns, duration);
    }

    pub(crate) fn locator_recount_duration(&self, duration: Duration) {
        Self::add_duration(&self.locator_recount_ns, duration);
    }

    pub(crate) fn bulk_finalization_duration(&self, duration: Duration) {
        Self::add_duration(&self.bulk_finalization_ns, duration);
    }

    pub(crate) fn snapshot(&self) -> PackedStoreTraceSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        PackedStoreTraceSnapshot {
            object_gets: load(&self.object_gets),
            object_get_bytes: load(&self.object_get_bytes),
            object_put_batches: load(&self.object_put_batches),
            object_put_attempts: load(&self.object_put_attempts),
            object_put_bytes: load(&self.object_put_bytes),
            object_gets_by_kind: std::array::from_fn(|index| {
                load(&self.object_gets_by_kind[index])
            }),
            content_gets: load(&self.content_gets),
            content_get_bytes: load(&self.content_get_bytes),
            content_put_batches: load(&self.content_put_batches),
            content_put_attempts: load(&self.content_put_attempts),
            content_put_bytes: load(&self.content_put_bytes),
            catalog_gets: load(&self.catalog_gets),
            catalog_bulk_hits: load(&self.catalog_bulk_hits),
            catalog_locator_hits: load(&self.catalog_locator_hits),
            deferred_put_calls: load(&self.deferred_put_calls),
            deferred_put_attempts: load(&self.deferred_put_attempts),
            deferred_put_duplicates: load(&self.deferred_put_duplicates),
            deferred_appends: load(&self.deferred_appends),
            deferred_index_transactions: load(&self.deferred_index_transactions),
            locator_recounts: load(&self.locator_recounts),
            bulk_rolls: load(&self.bulk_rolls),
            bulk_finalizations: load(&self.bulk_finalizations),
            catalog_lookup_ns: load(&self.catalog_lookup_ns),
            deferred_append_ns: load(&self.deferred_append_ns),
            deferred_index_transaction_ns: load(&self.deferred_index_transaction_ns),
            locator_recount_ns: load(&self.locator_recount_ns),
            bulk_finalization_ns: load(&self.bulk_finalization_ns),
        }
    }
}
