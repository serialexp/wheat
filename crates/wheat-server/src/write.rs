//! Writes an embedder starts, with no client session: Gothab's ref writes, edits
//! and merges, for example. A write takes its turn in the repository's group commit
//! as a publish does, so it is serialized with live publishes and made durable in
//! their batches, and its key makes it idempotent as a publish key does (see
//! [`crate::receipt`]). No publish hooks run: the embedder tells its own listeners.
//!
//! [`write()`] changes labels alone. An edit or a merge is built first, in a
//! [`ServerStage`] opened on the durable head: [`ServerStage::build`] gives the
//! store's [`PersistedRepositoryMutator`] over the stage, whose `publish` moves the
//! stage's tip in memory only. [`ServerStage::submit`] then joins the tip onto the
//! head as a publish from the stage's base is, and makes the write's label changes
//! there.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use tokio::time::Instant;
use tracing::{Instrument as _, debug_span, warn};
pub use wheat_core::ExpectedLabel;
use wheat_core::{
    ViewId, ViewLabel, ViewLabelManifestRecord, server_stage_request_digest,
    server_write_request_digest,
};
use wheat_store::{
    PUBLISH_RECEIPT_RETENTION_SECONDS, PersistedRepositoryMutator, PublicationError,
    PublicationStore, RepositoryHead, RepositoryReader, RepositoryStage,
};

use crate::{
    access::Principal,
    commit::{self, Built, CommitFailure, LabelRefusal},
    error_text::chain,
    heads::ReceiptLookup,
    naming::Named,
    network::Network,
    quiesce::Writing,
    receipt::PublishKey,
    registry::RepositoryEntry,
};

/// The most objects, and bytes, a server write stages: its label manifest and
/// envelope.
const STAGED_OBJECTS: usize = 16;
const STAGED_BYTES: usize = 16 * 1024 * 1024;

/// The most objects, and bytes, a [`ServerStage`] holds before storing them early:
/// as many as a publish's stage.
pub(crate) const STAGE_OBJECTS: usize = 65_536;
pub(crate) const STAGE_BYTES: usize = 64 * 1024 * 1024;

/// One label a write changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelChange {
    pub name: String,
    /// What the label must be on the head the write joins onto.
    pub expected: ExpectedLabel,
    /// What the label becomes; `None` deletes it. A [`ViewLabel`], so a tag keeps its
    /// annotation.
    pub new: Option<ViewLabel>,
}

/// A write of a repository's labels.
#[derive(Clone, Debug)]
pub struct ServerWrite {
    /// Who the write is for: receipts are kept per principal and key.
    pub principal: Principal,
    /// The embedder's idempotency key. The same key with the same changes gets the
    /// first answer; with other changes it is refused.
    pub key: [u8; 16],
    pub labels: Vec<LabelChange>,
    /// As for a publish: otherwise every label moved must move forward.
    pub force_labels: bool,
}

/// Why a write was not made.
#[derive(Clone, Debug)]
pub enum WriteFailure {
    /// The changes are not a write: none, a label named twice, one expected absent
    /// and deleted, or a label that is not valid.
    Invalid(String),
    /// Labels are not where the write expects them: each with its view on the head
    /// the write would have joined onto (none when absent). `head` is the durable
    /// head.
    LabelMoved {
        head: RepositoryHead,
        labels: Vec<(String, Option<ViewId>)>,
    },
    /// The network refuses writes for the moment (it is being exported or moved);
    /// try again after `retry_after` when it says.
    Quiesced { retry_after: Option<Duration> },
    /// As [`CommitFailure`]: refused, or the store failed.
    Commit(CommitFailure),
}

/// Makes `write` to the repository `entry` and answers the head it was published in.
#[tracing::instrument(name = "server_write", level = "debug", skip_all)]
pub async fn write(
    entry: &Arc<RepositoryEntry>,
    write: ServerWrite,
) -> Result<RepositoryHead, WriteFailure> {
    let request = request_digest(&write)?;
    // Held until the write is answered, so a quiesce waits for it.
    let writing = begin(entry)?;
    let staged = Arc::new(RepositoryStage::new(
        entry.network.clone(),
        STAGED_OBJECTS,
        STAGED_BYTES,
    ));
    let answer = accept(entry, &staged, write, request, None).await;
    drop(writing);
    answer
}

/// A slot among the repository's writers, or why there is none.
pub(crate) fn begin(entry: &RepositoryEntry) -> Result<Writing, WriteFailure> {
    entry
        .network
        .writers
        .begin()
        .ok_or_else(|| WriteFailure::Quiesced {
            retry_after: entry.network.writers.retry_after(),
        })
}

/// Accepts `write`, asking for `request`, with what a stage `built`, and waits for its
/// batch.
async fn accept(
    entry: &Arc<RepositoryEntry>,
    staged: &Arc<RepositoryStage<Network>>,
    write: ServerWrite,
    request: [u8; 16],
    built: Option<Built>,
) -> Result<RepositoryHead, WriteFailure> {
    let key = PublishKey::server_write(&write.principal, write.key, request)
        .map_err(|e| WriteFailure::Invalid(format!("the principal cannot be bound: {e}")))?;
    let accepted = commit::accept_labels(
        entry,
        staged,
        key,
        Arc::new(write.labels),
        write.force_labels,
        built,
    )
    .await
    .map_err(|refusal| match refusal {
        LabelRefusal::Moved { head, labels } => WriteFailure::LabelMoved { head, labels },
        LabelRefusal::Failed(failure) => WriteFailure::Commit(failure),
    })?;
    accepted.durable().await.map_err(WriteFailure::Commit)
}

/// Where an edit or a merge is built before it is submitted: a repository stage
/// opened on the durable head, whose tip is that head until a build publishes into
/// it. It holds a slot among the repository's writers until it is submitted or
/// dropped, so a quiesce waits for it; keep it for one request. Dropped unsubmitted,
/// it leaves what it stored early, and the content its builds wrote, for a collection
/// to reclaim.
pub struct ServerStage {
    entry: Arc<RepositoryEntry>,
    inner: Arc<StageInner>,
    /// Before anything was written into the stage: each stamp of its content is newer.
    opened: Instant,
    /// What the tip last submitted holds, kept for submitting it again: the objects
    /// staged for it go with the write once it is accepted.
    submitted: tokio::sync::Mutex<Option<(RepositoryHead, BuiltState)>>,
    _writing: Writing,
}

/// The content a stage's tip needs over its base's, when the network is told what
/// publishes rely on.
#[derive(Clone)]
struct BuiltState {
    needed: Vec<wheat_core::ContentId>,
}

struct StageInner {
    stage: Arc<RepositoryStage<Network>>,
    base: RepositoryHead,
    tip: StageHead,
}

/// A stage's tip, the head its builds read and publish to. Publishing to it moves it
/// in memory; nothing about it is durable.
#[derive(Debug)]
pub struct StageHead(Mutex<RepositoryHead>);

impl StageHead {
    fn get(&self) -> RepositoryHead {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PublicationStore for StageHead {
    fn read(&self) -> Result<RepositoryHead, PublicationError> {
        Ok(self.get())
    }

    fn initialize(&self, _: RepositoryHead) -> Result<(), PublicationError> {
        Err(PublicationError::Io(std::io::Error::other(
            "a stage starts on the head it was opened on",
        )))
    }

    fn compare_and_advance(
        &self,
        expected: RepositoryHead,
        proposed: RepositoryHead,
    ) -> Result<(), PublicationError> {
        let mut tip = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if *tip != expected {
            return Err(PublicationError::Stale {
                expected,
                actual: *tip,
            });
        }
        if proposed.generation != expected.generation + 1 {
            return Err(PublicationError::InvalidGeneration);
        }
        *tip = proposed;
        Ok(())
    }
}

/// What a build is given: the stage, to read and to mutate.
pub struct StageBuilder {
    inner: Arc<StageInner>,
}

/// The mutator a build is given, over the stage and its tip.
pub type StageMutator<'a> = PersistedRepositoryMutator<'a, RepositoryStage<Network>, StageHead>;

impl StageBuilder {
    /// The stage's tip: what a mutation is `expected` to build on.
    #[must_use]
    pub fn head(&self) -> RepositoryHead {
        self.inner.tip.get()
    }

    /// The store's mutator over the stage; its `publish` moves the tip.
    #[must_use]
    pub fn mutator(&self) -> StageMutator<'_> {
        PersistedRepositoryMutator::new(
            self.inner.stage.as_ref(),
            self.inner.stage.content(),
            &self.inner.tip,
        )
    }

    /// The stage read: the durable store with what was built into it.
    #[must_use]
    pub fn reader(&self) -> RepositoryReader<'_, RepositoryStage<Network>> {
        RepositoryReader::new(self.inner.stage.as_ref(), self.inner.stage.content())
    }

    /// The network's content, where a build puts what its edits write. A put is
    /// stored at once, not staged.
    #[must_use]
    pub fn content(&self) -> &wheat_store::ContentStore {
        self.inner.stage.content()
    }
}

/// Opens a stage on `entry`'s durable head.
#[tracing::instrument(name = "server_stage_open", level = "debug", skip_all)]
pub async fn open_stage(entry: &Arc<RepositoryEntry>) -> Result<ServerStage, WriteFailure> {
    let writing = begin(entry)?;
    let opened = Instant::now();
    let base = entry.heads.head().await.map_err(|e| {
        WriteFailure::Commit(CommitFailure::Internal(format!(
            "server stage: read the head: {}",
            chain(&e)
        )))
    })?;
    Ok(ServerStage {
        entry: Arc::clone(entry),
        inner: Arc::new(StageInner {
            stage: Arc::new(RepositoryStage::new(
                entry.network.clone(),
                STAGE_OBJECTS,
                STAGE_BYTES,
            )),
            base,
            tip: StageHead(Mutex::new(base)),
        }),
        opened,
        submitted: tokio::sync::Mutex::new(None),
        _writing: writing,
    })
}

impl ServerStage {
    /// The durable head the stage was opened on.
    #[must_use]
    pub fn base(&self) -> RepositoryHead {
        self.inner.base
    }

    /// The stage's tip: the base, or what a build last published into it.
    #[must_use]
    pub fn head(&self) -> RepositoryHead {
        self.inner.tip.get()
    }

    /// Runs `work` with a [`StageBuilder`] on a thread for blocking work, since the
    /// stores read and write the disk synchronously, and answers what it returns.
    pub async fn build<T, F, Fut>(&self, work: F) -> Result<T, WriteFailure>
    where
        F: FnOnce(StageBuilder) -> Fut + Send + 'static,
        Fut: Future<Output = T>,
        T: Send + 'static,
    {
        let builder = StageBuilder {
            inner: Arc::clone(&self.inner),
        };
        crate::blocking::drive(move || work(builder))
            .await
            .map_err(|e| {
                WriteFailure::Commit(CommitFailure::Internal(format!(
                    "server stage: the build stopped: {e}"
                )))
            })
    }
}

impl ServerStage {
    /// Submits what the stage built with `write`'s label changes, and answers the
    /// head they were published in. The stage's tip is joined onto the head the write
    /// joins onto as a publish from the stage's base would be, so publishes accepted
    /// since join it; a change both recorded differently is [`CommitFailure::Stale`].
    /// The labels move as [`write()`] moves them, there. The tip must keep the base's
    /// labels: labels move in the write alone.
    ///
    /// `intent` is the embedder's digest of what it asks for: the operation it built,
    /// with the inputs it built it from, and the labels it moves, with what it
    /// expects each to be. The receipt binds `write.key` to `intent` alone, since a
    /// stage's objects differ each time it is built. A submit whose key was published
    /// with that intent, by this stage or one built for it before, gets that head
    /// without joining anything ([`published`]); with another intent it is refused.
    /// A submit refused before it was accepted (labels moved, stale, rejected, its
    /// content not named) may be made again on the same stage; one whose batch failed
    /// took the stage's objects with it, so build again in a new stage.
    ///
    /// When the network is told what publishes rely on, what the built state adds
    /// over the base is named first ([`crate::naming`]); a failure is
    /// [`CommitFailure::Failed`].
    #[tracing::instrument(name = "server_stage_submit", level = "debug", skip_all)]
    pub async fn submit(
        &self,
        write: ServerWrite,
        intent: [u8; 32],
    ) -> Result<RepositoryHead, WriteFailure> {
        check(&write)?;
        let request = server_stage_request_digest(intent);
        // A receipt the head store may have trimmed is not an answer: only
        // `published` can say so, and a fresh submit to a store that is full looks
        // the same.
        if let (Some(head), _) = previous(&self.entry, &write.principal, write.key, request).await?
        {
            return Ok(head);
        }
        let base = self.inner.base;
        let tip = self.inner.tip.get();
        let mut submitted = self.submitted.lock().await;
        let built = match &*submitted {
            Some((at, built)) if *at == tip => built.clone(),
            _ => {
                let inner = Arc::clone(&self.inner);
                let naming = self.entry.network.naming.is_some();
                let built = crate::blocking::drive(move || async move {
                    built(&inner, base, tip, naming).await
                })
                .await
                .map_err(|e| {
                    WriteFailure::Commit(CommitFailure::Internal(format!(
                        "server stage: read what was built: {e}"
                    )))
                })??;
                *submitted = Some((tip, built.clone()));
                built
            }
        };
        let named = match &self.entry.network.naming {
            None => None,
            Some(naming) => {
                if let Err(e) = naming
                    .named(&built.needed)
                    .instrument(debug_span!("server_stage_name", ids = built.needed.len()))
                    .await
                {
                    warn!(error = %e, "server stage: naming its content failed");
                    return Err(WriteFailure::Commit(CommitFailure::Failed {
                        head: base,
                        message: format!(
                            "naming the built content failed: {e}; submit the stage again"
                        ),
                    }));
                }
                Some(Named {
                    since: self.opened,
                    ids: built.needed,
                })
            }
        };
        let built = Built {
            base: base.envelope,
            tip: tip.envelope,
            named,
        };
        accept(&self.entry, &self.inner.stage, write, request, Some(built)).await
    }
}

/// The stage's `tip`, checked to keep `base`'s labels: when `naming`, the content its
/// state needs over `base`'s, closure included.
async fn built(
    inner: &StageInner,
    base: RepositoryHead,
    tip: RepositoryHead,
    naming: bool,
) -> Result<BuiltState, WriteFailure> {
    let internal = |what: &str, e: &dyn std::error::Error| {
        WriteFailure::Commit(CommitFailure::Internal(format!(
            "server stage: {what}: {}",
            chain(e)
        )))
    };
    let reader = RepositoryReader::new(inner.stage.as_ref(), inner.stage.content());
    let before = reader
        .envelope(base.envelope)
        .await
        .map_err(|e| internal("read the base", &e))?;
    let after = reader
        .envelope(tip.envelope)
        .await
        .map_err(|e| internal("read the tip", &e))?;
    if after.labels != before.labels {
        return Err(WriteFailure::Invalid(
            "a stage keeps its base's labels: move labels in the write".into(),
        ));
    }
    if !naming {
        return Ok(BuiltState { needed: Vec::new() });
    }
    let needed = needed_over(&inner.stage, before.state, after.state)
        .await
        .map_err(|e| WriteFailure::Commit(CommitFailure::Internal(format!("server stage: {e}"))))?;
    Ok(BuiltState { needed })
}

/// The content a head moving from the state `before` to `after` relies on that
/// `before` did not: what `after` requires over `before`, with everything reading it
/// needs. What a network that is told what publishes rely on is told before such a
/// move joins the commit.
pub(crate) async fn needed_over(
    stage: &RepositoryStage<Network>,
    before: wheat_core::RepositoryStateId,
    after: wheat_core::RepositoryStateId,
) -> Result<Vec<wheat_core::ContentId>, String> {
    let reader = RepositoryReader::new(stage, stage.content());
    let reader = &reader;
    let required = |state| async move {
        reader
            .state(state)
            .await
            .map(|state| state.required_content)
            .map_err(|e| format!("read a state: {}", chain(&e)))
    };
    let added = wheat_store::ContentSetStore::new(stage)
        .added(required(before).await?, required(after).await?)
        .await
        .map_err(|e| format!("compare the content sets: {}", chain(&e)))?;
    stage
        .content()
        .needed_to_read(added, [])
        .map_err(|e| format!("find what the content needs: {}", chain(&e)))
}

/// What [`published`] found of a stage submit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Submission {
    /// The submit was published in this head.
    Published(RepositoryHead),
    /// It was not published, or its receipt has expired (see [`published`]).
    NotPublished,
    /// The head store may have dropped its receipt early, to stay within its
    /// [`HeadStore::receipt_limit`](crate::heads::HeadStore::receipt_limit): whether it
    /// was published cannot be told. Submitting it again may apply it twice.
    Unknown,
}

/// What became of a stage submit by `principal` with `key` and `intent`, waiting for
/// its batch if it is in one. Lets an embedder that lost the answer, as when it
/// crashed after the submit, find it before building the stage again. A key published
/// with another intent is refused as reused.
///
/// A head keeps receipts for [`wheat_store::PUBLISH_RECEIPT_RETENTION_SECONDS`]: once
/// a submit's receipt has expired, this answers [`Submission::NotPublished`] whether
/// or not it was published, so an embedder's retries must come sooner. A head store
/// with a receipt limit may drop one sooner, and this then answers
/// [`Submission::Unknown`] rather than `NotPublished`.
pub async fn published(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    key: [u8; 16],
    intent: [u8; 32],
) -> Result<Submission, WriteFailure> {
    let request = server_stage_request_digest(intent);
    Ok(match previous(entry, principal, key, request).await? {
        (Some(head), _) => Submission::Published(head),
        (None, ReceiptLookup::Trimmed { oldest_kept })
            if commit::unix_seconds().saturating_sub(oldest_kept)
                < PUBLISH_RECEIPT_RETENTION_SECONDS =>
        {
            Submission::Unknown
        }
        // Whatever was dropped was recorded before the oldest receipt kept, so it
        // would have expired by now anyway.
        (None, _) => Submission::NotPublished,
    })
}

/// The head the write by `principal` with `key` asking for `request` was published
/// in, waiting for it if it is still in a batch, and what the head store held under
/// the key.
async fn previous(
    entry: &Arc<RepositoryEntry>,
    principal: &Principal,
    key: [u8; 16],
    request: [u8; 16],
) -> Result<(Option<RepositoryHead>, ReceiptLookup), WriteFailure> {
    let key = PublishKey::server_write(principal, key, request)
        .map_err(|e| WriteFailure::Invalid(format!("the principal cannot be bound: {e}")))?;
    let (durable, receipt) = entry
        .heads
        .head_with_receipt(key.key())
        .await
        .map_err(|e| {
            WriteFailure::Commit(CommitFailure::Internal(format!(
                "server write: read the head: {}",
                chain(&e)
            )))
        })?;
    match commit::previous(entry, durable, receipt.found(), &key)
        .await
        .map_err(WriteFailure::Commit)?
    {
        Some(accepted) => accepted
            .durable()
            .await
            .map(|head| (Some(head), receipt))
            .map_err(WriteFailure::Commit),
        None => Ok((None, receipt)),
    }
}

/// What a label `write` asks for, digested, once its changes are checked to be a
/// write.
fn request_digest(write: &ServerWrite) -> Result<[u8; 16], WriteFailure> {
    check(write)?;
    Ok(server_write_request_digest(
        write
            .labels
            .iter()
            .map(|change| (change.name.as_str(), change.expected, change.new.as_ref())),
        write.force_labels,
    ))
}

/// Refuses changes that are not a write: none, a label named twice, one expected
/// absent and deleted, or a label that is not valid.
fn check(write: &ServerWrite) -> Result<(), WriteFailure> {
    if write.labels.is_empty() {
        return Err(WriteFailure::Invalid(
            "a write changes at least one label".into(),
        ));
    }
    let mut names = BTreeSet::new();
    for change in &write.labels {
        if !names.insert(change.name.as_str()) {
            return Err(WriteFailure::Invalid(format!(
                "label {:?} is changed twice",
                change.name
            )));
        }
        if change.expected == ExpectedLabel::Absent && change.new.is_none() {
            return Err(WriteFailure::Invalid(format!(
                "label {:?} neither exists before nor after the write",
                change.name
            )));
        }
        ViewLabelManifestRecord::validate_entry(&change.name, change.new.as_ref())
            .map_err(|e| WriteFailure::Invalid(format!("the labels are not valid: {e}")))?;
    }
    Ok(())
}
