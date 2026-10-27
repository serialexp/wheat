//! Hooks an embedder runs around a publish.
//!
//! [`PublishHooks::check`] is asked once a publish has uploaded everything and asked
//! to commit, before it is joined onto the head, so the embedder can refuse it
//! (protected labels, required checks, quota, size). It runs outside the
//! repository's commit lock: a slow check holds up only its own publish. What it
//! sees is what the sender asked for, from the head it built on to the envelope it
//! proposes; the head the publish lands on also holds whatever others published
//! meanwhile, joined in.
//!
//! [`PublishHooks::published`] is told once per durable batch (see
//! [`crate::commit`]): the head before and after, and each publish the batch made,
//! after the publishes' clients have their answers. Batches are told in the order
//! they became durable, and the next batch waits for the call, so a slow one holds
//! up the repository's publishes; an embedder with slow work (webhooks, CI) should
//! queue it. A process that stops between moving the head and the call loses that
//! notice: an embedder that must see every publish compares the head's generation
//! with the last one it was told about.

use std::sync::Arc;

use async_trait::async_trait;
use wheat_core::RepositoryEnvelopeId;
use wheat_store::{LabelChange, ReceivedBytes, RepositoryHead, StoredWrite};

use crate::access::Principal;

/// What an embedder runs around each publish. Both methods default to doing nothing.
#[async_trait]
pub trait PublishHooks: Send + Sync {
    /// Whether `publish` may go ahead. A refusal's message is sent to the client,
    /// and the publish is refused with nothing it uploaded kept.
    async fn check(&self, publish: &ProposedPublish) -> Result<(), PublishRefusal> {
        let _ = publish;
        Ok(())
    }

    /// `batch` is durable.
    async fn published(&self, batch: &PublishedBatch) {
        let _ = batch;
    }
}

/// A publish whose upload is complete, before it is joined onto the head.
#[derive(Clone, Debug)]
pub struct ProposedPublish {
    pub repository: String,
    pub principal: Principal,
    /// The head the client built on, as it named it.
    pub expected: RepositoryHead,
    /// The envelope the client proposes.
    pub proposed: RepositoryEnvelopeId,
    /// A forced publish replaces the head instead of joining it.
    pub force: bool,
    /// Labels may move to views that are not forward from where they were.
    pub force_labels: bool,
    /// The labels the publish adds, moves and removes: `expected` to `proposed`,
    /// sorted by name.
    pub labels: Vec<LabelChange>,
    /// What the client uploaded.
    pub received: ReceivedBytes,
}

/// Why an embedder refused a publish. The message goes to the client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRefusal {
    pub message: String,
}

impl PublishRefusal {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Publishes made durable together: one head change.
#[derive(Clone, Debug)]
pub struct PublishedBatch {
    pub repository: String,
    /// The head the batch replaced.
    pub before: RepositoryHead,
    /// The head it published.
    pub after: RepositoryHead,
    /// In the order they were accepted.
    pub publishes: Vec<Published>,
}

/// One publish of a durable batch.
#[derive(Clone, Debug)]
pub struct Published {
    /// What the publish was, as [`PublishHooks::check`] saw it.
    pub publish: ProposedPublish,
    /// What it added to the store: its objects and content the store did not hold.
    /// What joining it with other publishes added is not counted.
    pub stored: StoredWrite,
}

/// An accepted publish to tell `hooks` about once it is durable.
pub struct Announcement {
    pub hooks: Arc<dyn PublishHooks>,
    pub publish: ProposedPublish,
}
