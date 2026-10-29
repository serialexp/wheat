//! Holding a repository still, so its files can be copied while it is served.
//!
//! A publish writes to the store from when it starts uploading (a large one is
//! written early, in stage-sized batches) until its batch is durable, and the group
//! committer writes until no batch is left. Each holds a `Writing` meanwhile.
//! [`Registry::quiesce`](crate::registry::Registry::quiesce) refuses new publishes,
//! with a retryable error, and waits for every `Writing` to end: then nothing
//! writes to the repository until the [`Quiesced`] is dropped. Fetches go on.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;

use crate::registry::RepositoryEntry;

/// Who writes to a repository, and whether it is quiesced.
#[derive(Default)]
pub(crate) struct Writers {
    state: Arc<watch::Sender<WriterState>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct WriterState {
    /// How many [`Quiesced`] are held.
    quiesced: usize,
    /// How many [`Writing`] are held.
    writing: usize,
    /// When the latest end a [`Quiesced::expect`] still held gave is due.
    expected_end: Option<Instant>,
}

impl Writers {
    /// Starts a publish's writes, or `None` while the repository is quiesced.
    pub(crate) fn begin(&self) -> Option<Writing> {
        let mut began = false;
        self.state.send_if_modified(|state| {
            began = state.quiesced == 0;
            if began {
                state.writing += 1;
            }
            began
        });
        began.then(|| Writing {
            state: Arc::clone(&self.state),
        })
    }

    /// While quiesced, how long the holders expect it to last, if any said.
    pub(crate) fn retry_after(&self) -> Option<std::time::Duration> {
        let state = self.state.borrow();
        let left = state
            .expected_end?
            .saturating_duration_since(Instant::now());
        // A hold that has overrun its estimate has none to give.
        (state.quiesced > 0 && !left.is_zero()).then_some(left)
    }

    /// Whether a [`Quiesced`] holds the repository.
    pub(crate) fn is_quiesced(&self) -> bool {
        self.state.borrow().quiesced > 0
    }

    /// Starts writes on behalf of publishes already writing (the group committer),
    /// which a quiesce waits for rather than refuses.
    pub(crate) fn continue_writing(&self) -> Writing {
        self.state.send_modify(|state| state.writing += 1);
        Writing {
            state: Arc::clone(&self.state),
        }
    }
}

/// Writes in progress; see the module documentation.
#[must_use]
pub(crate) struct Writing {
    state: Arc<watch::Sender<WriterState>>,
}

impl Drop for Writing {
    fn drop(&mut self) {
        self.state.send_modify(|state| state.writing -= 1);
    }
}

/// A repository nothing writes to while this is held: publishes are refused with a
/// retryable error, and those that were writing have finished. It also keeps the
/// repository open. Dropping it lets publishes in again.
#[must_use = "the repository takes publishes again once this is dropped"]
pub struct Quiesced {
    entry: Arc<RepositoryEntry>,
}

impl Quiesced {
    /// Refuses new publishes to `entry` and waits for the ones writing to finish.
    /// Dropped before it is ready, it lets publishes in again.
    /// [`Registry::quiesce`](crate::registry::Registry::quiesce) finds the entry by
    /// name.
    pub async fn hold(entry: Arc<RepositoryEntry>) -> Self {
        entry
            .network
            .writers
            .state
            .send_modify(|state| state.quiesced += 1);
        let quiesced = Self { entry };
        let mut state = quiesced.entry.network.writers.state.subscribe();
        // The sender lives in the entry this holds, so the wait cannot fail.
        let _ = state.wait_for(|state| state.writing == 0).await;
        quiesced
    }

    /// Says the hold is expected to last `duration` from now; publishes refused
    /// meanwhile tell their clients how much of it is left. The latest end among the
    /// holds in force counts, and none is kept once the last hold ends.
    pub fn expect(&self, duration: std::time::Duration) {
        let end = Instant::now() + duration;
        self.entry.network.writers.state.send_if_modified(|state| {
            let later = state.expected_end.is_none_or(|current| end > current);
            if later {
                state.expected_end = Some(end);
            }
            later
        });
    }

    /// The repository held still.
    #[must_use]
    pub fn entry(&self) -> &Arc<RepositoryEntry> {
        &self.entry
    }
}

impl std::fmt::Debug for Quiesced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Quiesced").finish_non_exhaustive()
    }
}

impl Drop for Quiesced {
    fn drop(&mut self) {
        self.entry.network.writers.state.send_modify(|state| {
            state.quiesced -= 1;
            if state.quiesced == 0 {
                state.expected_end = None;
            }
        });
    }
}
