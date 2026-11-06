//! Naming content for an embedder that collects its content store by age.
//!
//! Gothab keeps every repository's content in one blob store and reclaims what no
//! head needs (see `docs/design/wheat-network-maintenance.md`, "Gothab's
//! content"). A collection marks what the heads it reads need; a publish committed
//! after that read relies on content the mark never saw. Most of it the publish
//! uploads, and a put stamps it; the rest it relies on without writing: content the
//! store held already, the bases its uploads may be stored as deltas of, and the
//! chunks and parts its uploaded manifests share with what is stored, with
//! everything reading those needs. The embedder stamps those when they are
//! [`ContentNaming::named`], and reclaims only content stamped before the mark began,
//! less a grace.
//!
//! A publish names them before it joins the commit, so it is refused, and can be
//! sent again, when one was reclaimed after the publish found it. A batch names its
//! publishes' content again as it commits when that was longer ago than
//! [`ContentNaming::renew_after`], half the grace, so every head committed after a
//! mark began has its content stamped after the mark's start less the grace.

use std::{io, time::Duration};

use async_trait::async_trait;
use tokio::time::Instant;
use wheat_core::ContentId;

/// What an embedder that collects its content store by age is told: the content a
/// publish relies on. Set on a network ([`crate::network::Network::with_naming`]).
#[async_trait]
pub trait ContentNaming: Send + Sync {
    /// How old a batch's naming may be as the batch commits before it names its
    /// content again: half the collection's grace.
    fn renew_after(&self) -> Duration;

    /// `ids`, each stored, are relied on by a publish about to commit: stamp each.
    /// Fails if one is not stored any more; the publish is then refused as failed,
    /// and sending it again uploads it.
    async fn named(&self, ids: &[ContentId]) -> io::Result<()>;
}

/// What a publish named, and its uploads, with the instant before it planned them:
/// each of their stamps is newer.
#[derive(Clone, Debug)]
pub struct Named {
    pub since: Instant,
    pub ids: Vec<ContentId>,
}

impl Named {
    /// Takes in `other`'s content: the batch's stamps are as new as the oldest.
    pub(crate) fn join(&mut self, other: Self) {
        self.since = self.since.min(other.since);
        self.ids.extend(other.ids);
    }
}
