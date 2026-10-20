//! Reading and advancing a repository head that holds publish receipts.
//!
//! The head keeps the receipts of recent remote publishes, so a server reads and
//! rewrites up to [`MAX_PUBLISH_RECEIPTS`] of them with every publish: it reads the
//! head when a publish is accepted and again to advance it, and each advance writes
//! and syncs the whole record. This compares an empty head with a full one. A
//! receipt is 96 bytes, so a full head is about 96 KiB.

use std::time::Instant;

use anyhow::Result;
use wheat_core::{
    MAX_PUBLISH_RECEIPTS, PUBLISH_RECEIPT_ENCODED_SIZE, PublishReceiptRecord,
    REPOSITORY_HEAD_ENCODED_SIZE, RepositoryEnvelopeId,
};
use wheat_store::{LocalPublicationStore, PublicationStore, RepositoryHead};

use super::{CaseContext, CaseOutput, Recorder};

pub(crate) const NAME: &str = "head_receipts";

pub(crate) fn run(context: &CaseContext) -> Result<CaseOutput> {
    let reads = context.scale.pick(2_000, 20);
    let advances = context.scale.pick(200, 5);
    let mut recorder = Recorder::new(NAME);
    recorder.parameter("reads", reads);
    recorder.parameter("advances", advances);
    recorder.parameter("max_receipts", MAX_PUBLISH_RECEIPTS);

    let now = 1_800_000_000;
    for (step, held) in [("empty", 0), ("full", MAX_PUBLISH_RECEIPTS)] {
        let root = tempfile::tempdir_in(&context.scratch)?;
        let path = root.path().join("repository.head");
        let store = LocalPublicationStore::new(&path);
        let mut head = RepositoryHead::initial(envelope(0));
        store.initialize(head)?;
        if held > 0 {
            let filled: Vec<_> = (0..held).map(|i| receipt(i as u64, head, now)).collect();
            let next = head.next(envelope(1))?;
            store.compare_and_advance_with_receipts(head, next, &filled, now)?;
            head = next;
        }

        let started = Instant::now();
        for _ in 0..reads {
            let (_, receipts) = store.read_with_receipts()?;
            assert_eq!(receipts.len(), held);
        }
        let read = started.elapsed().as_secs_f64() / reads as f64;

        // Each advance records one receipt, as a publish does; a full head drops its
        // oldest to stay full.
        let started = Instant::now();
        for index in 0..advances {
            let next = head.next(envelope(index as u64 + 2))?;
            if held == 0 {
                store.compare_and_advance(head, next)?;
            } else {
                let added = [receipt((held + index) as u64, next, now)];
                store.compare_and_advance_with_receipts(head, next, &added, now)?;
            }
            head = next;
        }
        let advance = started.elapsed().as_secs_f64() / advances as f64;
        let bytes = std::fs::metadata(&path)?.len();

        recorder.value(&format!("{step}.read_seconds"), read, "s");
        recorder.value(&format!("{step}.advance_seconds"), advance, "s");
        recorder.value(&format!("{step}.head_bytes"), bytes as f64, "bytes");
    }
    // A full head is the fixed part and the most receipts it may hold, nothing more.
    recorder.at_most(
        "full.head_bytes",
        (REPOSITORY_HEAD_ENCODED_SIZE + MAX_PUBLISH_RECEIPTS * PUBLISH_RECEIPT_ENCODED_SIZE) as f64,
        "bytes",
    );
    Ok(recorder.finish())
}

fn envelope(seed: u64) -> RepositoryEnvelopeId {
    let mut digest = [0; 32];
    digest[..8].copy_from_slice(&seed.to_le_bytes());
    RepositoryEnvelopeId::from_digest(digest)
}

fn receipt(seed: u64, head: RepositoryHead, now: u64) -> PublishReceiptRecord {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&seed.to_le_bytes());
    PublishReceiptRecord {
        key,
        request: key,
        receipt: key,
        generation: head.generation,
        envelope: head.envelope,
        recorded_at: now,
    }
}
