//! Group commit: publishes made durable together.
//!
//! A publish is joined onto the head and validated one at a time, as before, but is
//! then made durable together with the publishes accepted around it: one store
//! write and one head compare-and-swap per batch. While one batch is being written,
//! the publishes after it join onto its head (the tentative head), read its objects
//! from memory, and form the next batch. A batch that fails also fails the batch
//! built on it.
//!
//! The head records each publish's receipt in the write that publishes it. A publish
//! sent again with its key (see [`crate::receipt`]) is not accepted a second time: it
//! gets the receipt, or waits for the batch the first sending is in.

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use tokio::sync::{Mutex, oneshot};
use tracing::{Instrument as _, debug_span, warn};
use wheat_core::{PublishReceiptRecord, RepositoryEnvelopeId};
use wheat_store::{
    Arrival, ImmutableObjectStore, PendingPublications, PublicationError, PublicationStore,
    RepositoryHead, RepositoryPublicationError, RepositoryReader, RepositoryStage, RepositoryStore,
    RepositoryStoreError, RepositoryUnionError, StagedPublication,
};

use crate::receipt::{PublishKey, Recorded};
use crate::registry::RepositoryEntry;

/// Times a batch retries its compare-and-swap when another process sharing the
/// store moved the head in between. Batches through this server take turns, so they
/// never cause a retry.
const MAX_COMMIT_ATTEMPTS: usize = 16;

/// Most objects, and separately most bytes, a batch's re-join stages before writing
/// them early.
const REJOIN_STAGED_OBJECTS: usize = 65_536;
const REJOIN_STAGED_BYTES: usize = 64 * 1024 * 1024;

/// Why a publish accepted into a batch was not published.
#[derive(Clone, Debug)]
pub enum CommitFailure {
    /// The head moved in a way the publish cannot be joined with: the client must
    /// fetch and publish again.
    Stale {
        head: RepositoryHead,
        message: String,
    },
    /// The batch joined with the head is not a valid transition.
    Rejected {
        head: RepositoryHead,
        message: String,
    },
    /// Storing the batch or advancing the head failed.
    Failed {
        head: RepositoryHead,
        message: String,
    },
    /// The head could not be read.
    Internal(String),
}

pub type CommitOutcome = Result<RepositoryHead, CommitFailure>;

/// A repository's publishes waiting to join, and the batches being made durable.
#[derive(Default)]
pub struct GroupCommit {
    state: Mutex<CommitState>,
}

#[derive(Default)]
struct CommitState {
    /// The last accepted publish's envelope while it is not yet durable: what the
    /// next publish joins onto instead of the durable head.
    tentative: Option<RepositoryEnvelopeId>,
    /// The batch being written, readable meanwhile.
    writing: PendingPublications,
    /// The keys of that batch's publishes, and who waits for it.
    writing_keys: Vec<PublishKey>,
    writing_members: Vec<oneshot::Sender<CommitOutcome>>,
    /// Publishes accepted since that batch was taken.
    next: Option<Batch>,
    /// Whether a committer task is running.
    committing: bool,
}

struct Batch {
    /// The envelope the batch's first publish joined onto.
    built_on: RepositoryEnvelopeId,
    /// The last publish's joined envelope: every publish of the batch joined.
    target: RepositoryEnvelopeId,
    /// Whether a forced publish is in the batch, so it must not be re-joined.
    forced: bool,
    publications: PendingPublications,
    /// The keys of the batch's publishes, whose receipts the head records.
    keys: Vec<PublishKey>,
    /// Who waits for the batch: its publishes, and any of them sent again.
    members: Vec<oneshot::Sender<CommitOutcome>>,
}

/// A publish accepted into a batch, or one sent again and answered as the first
/// sending was.
pub struct Accepted {
    answer: Answer,
    repeated: bool,
}

enum Answer {
    Waiting(oneshot::Receiver<CommitOutcome>),
    Published(RepositoryHead),
}

impl Accepted {
    fn waiting(receiver: oneshot::Receiver<CommitOutcome>, repeated: bool) -> Self {
        Self {
            answer: Answer::Waiting(receiver),
            repeated,
        }
    }

    /// Whether the publish was sent before with its key, so this is the first
    /// sending's answer.
    pub fn repeated(&self) -> bool {
        self.repeated
    }

    /// Waits for the publish's batch: the head it published, or why it did not.
    pub async fn durable(self) -> CommitOutcome {
        match self.answer {
            Answer::Published(head) => Ok(head),
            Answer::Waiting(receiver) => receiver
                .instrument(debug_span!("publish_durable_wait"))
                .await
                .unwrap_or_else(|_| {
                    Err(CommitFailure::Internal(
                        "publish: the group commit stopped".into(),
                    ))
                }),
        }
    }
}

/// The answer to a publish sent before with `key`, if it was: the head it was
/// published in, by the `durable` head's `receipts`, or the batch it waits in.
/// Checked before the publish's transfer, so a retry after the head moved is not
/// refused as stale and sends nothing again.
///
/// The head may be read before this is called. A first sending published since then
/// is no longer waiting in a batch, so it is not found here; [`accept`] reads the
/// head again and finds it.
pub async fn previous(
    entry: &Arc<RepositoryEntry>,
    durable: RepositoryHead,
    receipts: &[PublishReceiptRecord],
    key: &PublishKey,
) -> Result<Option<Accepted>, CommitFailure> {
    let mut turn = entry.commit.state.lock().await;
    turn.previous(receipts, durable, key).transpose()
}

/// Joins `proposed`, built on `expected`, with whatever was accepted since, and
/// accepts it into the next batch, taking what `staged` holds.
///
/// The join is the one a fetch does: the union of both states, with labels merged
/// three ways against `expected`. Only a label both sides moved, or a change
/// recorded differently on each side, refuses the publish as stale. A forced publish
/// is never joined: it replaces exactly the head it names. Refusals report the
/// durable head, the one the client can fetch.
///
/// A publish whose key was accepted meanwhile, by another session sending it again,
/// is answered as that one is instead.
#[tracing::instrument(name = "publish_accept", level = "debug", skip_all)]
pub async fn accept<S: RepositoryStore + ?Sized>(
    entry: &Arc<RepositoryEntry>,
    staged: &RepositoryStage<'_, S>,
    key: PublishKey,
    expected: RepositoryEnvelopeId,
    proposed: RepositoryEnvelopeId,
    force: bool,
) -> Result<Accepted, CommitFailure> {
    let mut turn = entry
        .commit
        .state
        .lock()
        .instrument(debug_span!("publish_commit_wait"))
        .await;
    let (durable, receipts) = read_receipts(entry)?;
    if let Some(previous) = turn.previous(&receipts, durable, &key) {
        return previous;
    }
    let current = turn.tentative.unwrap_or(durable.envelope);
    let pending = turn.pending();
    let view = pending.over(staged);
    let reader = RepositoryReader::new(&view, view.content());
    let joined = if current == expected {
        proposed
    } else if force {
        return Err(CommitFailure::Stale {
            head: durable,
            message: "a forced publish must name the current head".to_string(),
        });
    } else {
        reader
            .union_envelopes(Some(expected), proposed, current)
            .instrument(debug_span!("publish_join"))
            .await
            .map_err(|e| join_failure(e, durable))?
    };
    match validate(&reader, current, joined, force).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(CommitFailure::Rejected {
                head: durable,
                message: format!("validation failed: {e}"),
            });
        }
        Err(e) => return Err(CommitFailure::Internal(e)),
    }
    let publication = staged.take().await.map_err(|e| {
        CommitFailure::Internal(format!("publish: take the staged publication: {e}"))
    })?;
    // Nothing above changed the state, so a publish dropped while joining (its client
    // went away) leaves no trace.
    let receiver = turn.accept(entry, key, current, joined, force, publication);
    Ok(Accepted::waiting(receiver, false))
}

/// The refusal of a publish whose key was used for a different one.
fn reused(head: RepositoryHead) -> CommitFailure {
    CommitFailure::Rejected {
        head,
        message: "this publish key was used for a different publish".to_string(),
    }
}

/// The answer to `key` if a publish in `keys`, whose batch `members` wait for, has it.
fn wait_in(
    keys: &[PublishKey],
    members: &mut Vec<oneshot::Sender<CommitOutcome>>,
    key: &PublishKey,
    durable: RepositoryHead,
) -> Option<Result<Accepted, CommitFailure>> {
    let first = keys.iter().find(|other| other.same_key(key))?;
    if !first.same_request(key) {
        return Some(Err(reused(durable)));
    }
    let (sender, receiver) = oneshot::channel();
    members.push(sender);
    Some(Ok(Accepted::waiting(receiver, true)))
}

/// Why `head` and a publish could not be joined.
fn join_failure(error: RepositoryUnionError, head: RepositoryHead) -> CommitFailure {
    match error {
        e @ (RepositoryUnionError::DifferentBaseView { .. }
        | RepositoryUnionError::DivergentChanges(_)
        | RepositoryUnionError::DivergentLabels(_)) => CommitFailure::Stale {
            head,
            message: format!("{e}; fetch and resolve, then publish again"),
        },
        e => CommitFailure::Internal(format!("publish: join with the current head: {e}")),
    }
}

impl CommitState {
    /// The answer to a publish sent before with `key`: from the durable head's
    /// `receipts`, or by waiting for the batch the first sending is in.
    fn previous(
        &mut self,
        receipts: &[PublishReceiptRecord],
        durable: RepositoryHead,
        key: &PublishKey,
    ) -> Option<Result<Accepted, CommitFailure>> {
        match key.find(receipts) {
            Some(Recorded::Published(head)) => {
                return Some(Ok(Accepted {
                    answer: Answer::Published(head),
                    repeated: true,
                }));
            }
            Some(Recorded::Reused) => return Some(Err(reused(durable))),
            None => {}
        }
        if let Some(answer) = wait_in(&self.writing_keys, &mut self.writing_members, key, durable) {
            return Some(answer);
        }
        let next = self.next.as_mut()?;
        wait_in(&next.keys, &mut next.members, key, durable)
    }

    /// The accepted publications not yet durable, to read beneath a publish's stage.
    fn pending(&self) -> PendingPublications {
        let mut pending = self.writing.clone();
        if let Some(next) = &self.next {
            pending.extend(&next.publications);
        }
        pending
    }

    /// Accepts a publish whose envelope, joined onto `built_on`, is `joined`. It
    /// becomes the tentative head and joins the next batch; the receiver gets the
    /// head that batch publishes.
    fn accept(
        &mut self,
        entry: &Arc<RepositoryEntry>,
        key: PublishKey,
        built_on: RepositoryEnvelopeId,
        joined: RepositoryEnvelopeId,
        forced: bool,
        publication: Arc<StagedPublication>,
    ) -> oneshot::Receiver<CommitOutcome> {
        let (sender, receiver) = oneshot::channel();
        debug_assert_eq!(self.tentative.unwrap_or(built_on), built_on);
        let batch = self.next.get_or_insert_with(|| Batch {
            built_on,
            target: joined,
            forced: false,
            publications: PendingPublications::default(),
            keys: Vec::new(),
            members: Vec::new(),
        });
        batch.target = joined;
        batch.forced |= forced;
        batch.publications.push(publication);
        batch.keys.push(key);
        batch.members.push(sender);
        self.tentative = Some(joined);
        if !self.committing {
            self.committing = true;
            // A thread of its own, so a publish whose client goes away does not stop
            // the batches of the others, and so the committer's writes and syncs
            // block a thread of the blocking pool rather than a runtime worker.
            let runtime = tokio::runtime::Handle::current();
            let entry = entry.clone();
            tokio::task::spawn_blocking(move || runtime.block_on(commit_batches(entry)));
        }
        receiver
    }
}

/// Makes batches durable, oldest first, until none is waiting.
async fn commit_batches(entry: Arc<RepositoryEntry>) {
    let mut abandoned = Abandoned(Some(entry.clone()));
    loop {
        let batch = {
            let mut state = entry.commit.state.lock().await;
            let Some(mut batch) = state.next.take() else {
                *state = CommitState::default();
                abandoned.0 = None;
                return;
            };
            state.writing = batch.publications.clone();
            state.writing_keys = batch.keys.clone();
            state.writing_members = std::mem::take(&mut batch.members);
            batch
        };
        let outcome = commit_batch(&entry, &batch)
            .instrument(debug_span!("group_commit", publishes = batch.keys.len()))
            .await;
        let mut state = entry.commit.state.lock().await;
        // Once published, the head's receipts answer a publish sent again.
        state.writing_keys.clear();
        let members = std::mem::take(&mut state.writing_members);
        if let Err(failure) = &outcome {
            // The next batch joined onto this one's head, so it fails too.
            state.writing = PendingPublications::default();
            state.tentative = None;
            if let Some(next) = state.next.take() {
                let failed = match failure.head() {
                    Some(head) => CommitFailure::Stale {
                        head,
                        message: "a publish joined before this one was not published; \
                                  publish again"
                            .to_string(),
                    },
                    None => failure.clone(),
                };
                for member in next.members {
                    let _ = member.send(Err(failed.clone()));
                }
            }
        }
        drop(state);
        for member in members {
            // A publish whose client went away no longer waits.
            let _ = member.send(outcome.clone());
        }
    }
}

/// Clears the commit state when the committer stops before it is done, by panicking,
/// so the publishes waiting on it fail instead of waiting forever and the next
/// publish starts a new committer.
struct Abandoned(Option<Arc<RepositoryEntry>>);

impl Drop for Abandoned {
    fn drop(&mut self) {
        let Some(entry) = self.0.take() else {
            return;
        };
        // The committer holds the lock only briefly, so it is almost always free;
        // otherwise clear the state once the unwinding releases it.
        if let Ok(mut state) = entry.commit.state.try_lock() {
            *state = CommitState::default();
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                *entry.commit.state.lock().await = CommitState::default();
            });
        }
    }
}

impl CommitFailure {
    fn head(&self) -> Option<RepositoryHead> {
        match self {
            Self::Stale { head, .. } | Self::Rejected { head, .. } | Self::Failed { head, .. } => {
                Some(*head)
            }
            Self::Internal(_) => None,
        }
    }
}

/// Stores `batch` and moves the head to it, joined with whatever another process
/// published meanwhile.
async fn commit_batch(entry: &RepositoryEntry, batch: &Batch) -> CommitOutcome {
    // The objects and content in one write, then any buffered objects sealed into a
    // durable batch (e.g. a packed segment uploaded to S3) before the head can name
    // them. Sealing is a no-op for local stores.
    let stored = async {
        batch.publications.store(&*entry.store).await?;
        entry.store.seal().await?;
        Ok::<_, RepositoryStoreError>(())
    }
    .instrument(debug_span!("publish_store"))
    .await;
    if let Err(e) = stored {
        warn!(error = %e, "publish: storing a batch failed");
        return Err(CommitFailure::Failed {
            head: read_head(entry)?,
            message: format!("storing the published objects failed: {e}"),
        });
    }

    let mut built_on = batch.built_on;
    let mut target = batch.target;
    for _ in 0..MAX_COMMIT_ATTEMPTS {
        let head = read_head(entry)?;
        if head.envelope != built_on {
            // Another process sharing the store moved the head, or an earlier batch
            // was joined with such a move: join again.
            if batch.forced {
                return Err(CommitFailure::Stale {
                    head,
                    message: "a forced publish must name the current head".to_string(),
                });
            }
            target = rejoin(entry, built_on, target, head)
                .instrument(debug_span!("publish_rejoin"))
                .await?;
            built_on = head.envelope;
        }
        let new_head = RepositoryHead {
            generation: head.generation + 1,
            envelope: target,
        };
        let now = unix_seconds();
        let receipts: Vec<_> = batch
            .keys
            .iter()
            .map(|key| key.receipt(new_head, now))
            .collect();
        let advanced = debug_span!("publish_cas").in_scope(|| {
            entry
                .heads
                .compare_and_advance_with_receipts(head, new_head, &receipts, now)
        });
        match advanced {
            Ok(()) => return Ok(new_head),
            Err(PublicationError::Stale { .. }) => continue,
            Err(e) => {
                return Err(CommitFailure::Failed {
                    head,
                    message: format!("head advance failed: {e}"),
                });
            }
        }
    }
    Err(CommitFailure::Stale {
        head: read_head(entry)?,
        message: "the head kept moving under this publish; try again".to_string(),
    })
}

/// `target`, built on `built_on`, joined with `head` and stored.
async fn rejoin(
    entry: &RepositoryEntry,
    built_on: RepositoryEnvelopeId,
    target: RepositoryEnvelopeId,
    head: RepositoryHead,
) -> Result<RepositoryEnvelopeId, CommitFailure> {
    let stage = RepositoryStage::new(&*entry.store, REJOIN_STAGED_OBJECTS, REJOIN_STAGED_BYTES);
    let reader = RepositoryReader::new(&stage, stage.content());
    let joined = reader
        .union_envelopes(Some(built_on), target, head.envelope)
        .await
        .map_err(|e| join_failure(e, head))?;
    match validate(&reader, head.envelope, joined, false).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return Err(CommitFailure::Rejected {
                head,
                message: format!("validation failed: {e}"),
            });
        }
        Err(e) => return Err(CommitFailure::Internal(e)),
    }
    let stored = async {
        stage.flush().await?;
        entry.store.seal().await?;
        Ok::<_, RepositoryStoreError>(())
    }
    .await;
    stored.map_err(|e| CommitFailure::Failed {
        head,
        message: format!("storing the published objects failed: {e}"),
    })?;
    Ok(joined)
}

fn read_head(entry: &RepositoryEntry) -> Result<RepositoryHead, CommitFailure> {
    entry
        .heads
        .read()
        .map_err(|e| CommitFailure::Internal(format!("publish: read the head: {e}")))
}

fn read_receipts(
    entry: &RepositoryEntry,
) -> Result<(RepositoryHead, Vec<PublishReceiptRecord>), CommitFailure> {
    entry
        .heads
        .read_with_receipts()
        .map_err(|e| CommitFailure::Internal(format!("publish: read the head: {e}")))
}

/// Seconds since the Unix epoch, which receipts are recorded at. A clock before the
/// epoch counts as the epoch: the receipts then expire early, never late.
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// Checks that `to` may follow `from`: what it adds, or the whole of it when forced.
/// The outer error is a failure to read the current envelope; the inner one says why
/// `to` is invalid.
#[tracing::instrument(name = "publish_validate", level = "debug", skip_all)]
async fn validate<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    from: RepositoryEnvelopeId,
    to: RepositoryEnvelopeId,
    force: bool,
) -> Result<Result<(), RepositoryPublicationError>, String> {
    reader
        .envelope(from)
        .await
        .map_err(|e| format!("publish: read the current envelope: {e}"))?;
    let arrival = if force {
        Arrival::Forced
    } else {
        Arrival::Extends(from)
    };
    Ok(reader.validate_arrival(to, arrival).await)
}
