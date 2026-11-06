//! A check an embedder runs against its own [`HeadStore`], from its tests, that the
//! store keeps receipts as the trait promises.
//!
//! It counts receipts rather than waiting out the retention: it records more than
//! [`MAX_PUBLISH_RECEIPTS`] of them at once and asks for the first again.

use std::time::{SystemTime, UNIX_EPOCH};

use wheat_core::{MAX_PUBLISH_RECEIPTS, PublishReceiptRecord, RepositoryEnvelopeId};
use wheat_store::RepositoryHead;

use super::{HeadStore, ReceiptLookup};

/// How many receipts each advance of the check records.
const RECEIPTS_PER_ADVANCE: usize = 64;

/// Checks that `store` keeps receipts as [`HeadStore`] promises, panicking with what it
/// does differently.
///
/// `store` must hold a head and no receipts, and be one the check may move freely: it
/// advances the head, recording [`MAX_PUBLISH_RECEIPTS`] + 1 receipts at the present
/// time, to envelopes that do not exist. It then checks that:
///
/// - each advance moves the head, and a receipt is found right after it is recorded;
/// - a key never recorded is [`ReceiptLookup::Absent`], or [`ReceiptLookup::Trimmed`]
///   once a store with a [`HeadStore::receipt_limit`] may have dropped receipts;
/// - the first receipt, still young, is found if the store has no limit, and is found
///   or reported trimmed, never absent, if it has one;
/// - a store with a limit dropped no receipt before holding more than its limit.
pub async fn check_head_store(store: &impl HeadStore) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the clock is after the Unix epoch")
        .as_secs();
    let limit = store.receipt_limit();
    let mut head = store.head().await.expect("the store holds a head");
    let never = [0xff; 16];
    let (at, held) = lookup(store, &never).await;
    assert_eq!(at, head, "the head read with a receipt is the head");
    assert_eq!(
        held,
        ReceiptLookup::Absent,
        "a store holding no receipts finds none"
    );

    let total = MAX_PUBLISH_RECEIPTS + 1;
    let mut recorded = Vec::with_capacity(total);
    let mut batch = Vec::with_capacity(RECEIPTS_PER_ADVANCE);
    // The first receipt is recorded alone, so it is the oldest.
    for size in std::iter::once(1).chain(std::iter::repeat(RECEIPTS_PER_ADVANCE)) {
        if recorded.len() == total {
            break;
        }
        let proposed = next(head);
        batch.clear();
        for _ in 0..size.min(total - recorded.len()) {
            batch.push(receipt(recorded.len() + batch.len(), proposed, now));
        }
        store
            .advance(head, proposed, &batch, now)
            .await
            .expect("the store advances from its own head");
        assert_eq!(
            store.head().await.expect("the store holds a head"),
            proposed,
            "an advance moves the head"
        );
        head = proposed;
        recorded.extend_from_slice(&batch);
        let last = batch.last().expect("each advance records receipts");
        assert_eq!(
            lookup(store, &last.key).await,
            (head, ReceiptLookup::Found(*last)),
            "a receipt is found right after it is recorded"
        );
        if limit.is_none_or(|limit| recorded.len() <= limit) {
            assert_eq!(
                lookup(store, &recorded[0].key).await.1,
                ReceiptLookup::Found(recorded[0]),
                "a store keeps every young receipt while it holds no more than its limit ({limit:?}); it holds {}",
                recorded.len()
            );
        }
    }

    let first = lookup(store, &recorded[0].key).await.1;
    let unrecorded = lookup(store, &never).await.1;
    match limit {
        None => {
            assert_eq!(
                first,
                ReceiptLookup::Found(recorded[0]),
                "a store with no receipt limit keeps a young receipt however many are newer ({total} were recorded)"
            );
            assert_eq!(
                unrecorded,
                ReceiptLookup::Absent,
                "a store with no receipt limit never reports a receipt trimmed"
            );
        }
        Some(limit) => {
            assert!(
                matches!(first, ReceiptLookup::Found(found) if found == recorded[0])
                    || matches!(first, ReceiptLookup::Trimmed { .. }),
                "a store with a receipt limit ({limit}) reports a young receipt it dropped as trimmed, not absent: {first:?}"
            );
            assert!(
                matches!(unrecorded, ReceiptLookup::Trimmed { .. }),
                "a store with a receipt limit ({limit}) that may have dropped young receipts reports a key it holds none for as trimmed: {unrecorded:?}"
            );
        }
    }
}

async fn lookup(store: &impl HeadStore, key: &[u8; 16]) -> (RepositoryHead, ReceiptLookup) {
    store
        .head_with_receipt(key)
        .await
        .expect("the store reads its head and receipts")
}

/// A head after `head`, at an envelope that does not exist.
fn next(head: RepositoryHead) -> RepositoryHead {
    let generation = head.generation + 1;
    RepositoryHead {
        generation,
        envelope: RepositoryEnvelopeId::from_digest(id("envelope", generation as usize)),
    }
}

/// The `index`th receipt the check records, of a publish in `head`.
fn receipt(index: usize, head: RepositoryHead, now: u64) -> PublishReceiptRecord {
    let mut key = [0; 16];
    key.copy_from_slice(&id("key", index)[..16]);
    PublishReceiptRecord {
        key,
        request: [1; 16],
        receipt: [2; 16],
        generation: head.generation,
        envelope: head.envelope,
        recorded_at: now,
    }
}
/// A digest standing for the `index`th `what` of the check.
fn id(what: &str, index: usize) -> [u8; 32] {
    *blake3::hash(format!("wheat head store check: {what} {index}").as_bytes()).as_bytes()
}
