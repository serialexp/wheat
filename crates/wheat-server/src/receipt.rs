//! Publish keys. A client sends each publish with a key and, when the connection is
//! lost before the answer arrives, sends it again with the same key. The server keeps
//! the receipts of recent publishes in the repository head, written in the same step
//! that moves the head, so the retry gets the first publish's answer instead of being
//! published twice or refused as stale.

use wheat_core::{
    PublishReceiptRecord, RepositoryEnvelopeId, publish_key_digest, publish_receipt_id,
    publish_request_digest,
};
use wheat_protocol::PublishRequestOutput;
use wheat_store::RepositoryHead;

use crate::access::Principal;
use crate::connection::ConnectionError;

/// A publish's key, bound to the principal that sent it, and a digest of what the
/// publish asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishKey {
    key: [u8; 16],
    request: [u8; 16],
}

/// What a head's receipts say about a publish key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Recorded {
    /// The publish was published in this head.
    Published(RepositoryHead),
    /// The key was used for a different publish.
    Reused,
}

impl PublishKey {
    /// Two principals sending the same key have different publish keys, so neither
    /// sees nor blocks the other's publish. Refuses a request whose key or envelope
    /// IDs have the wrong length.
    pub fn new(
        principal: &Principal,
        request: &PublishRequestOutput,
    ) -> Result<Self, ConnectionError> {
        let envelope = |bytes: &[u8], what| {
            bytes
                .try_into()
                .map(RepositoryEnvelopeId::from_digest)
                .map_err(|_| ConnectionError::Protocol(what))
        };
        let client_key = request
            .publish_key
            .bytes
            .as_slice()
            .try_into()
            .map_err(|_| ConnectionError::Protocol("invalid publish key"))?;
        let key = publish_key_digest(principal.name.as_bytes(), client_key)
            .map_err(|_| ConnectionError::Internal("principal name too long".into()))?;
        let request = publish_request_digest(
            envelope(&request.envelope_id.bytes, "invalid envelope ID")?,
            request.expected_generation,
            envelope(
                &request.expected_envelope.bytes,
                "invalid expected envelope ID",
            )?,
            request.force,
        );
        Ok(Self { key, request })
    }

    /// The receipt of this publish published in `head` at `now`, in seconds since the
    /// Unix epoch.
    pub fn receipt(&self, head: RepositoryHead, now: u64) -> PublishReceiptRecord {
        PublishReceiptRecord {
            key: self.key,
            request: self.request,
            receipt: self.receipt_id(head),
            generation: head.generation,
            envelope: head.envelope,
            recorded_at: now,
        }
    }

    /// The receipt ID the client is told for this publish published in `head`.
    pub fn receipt_id(&self, head: RepositoryHead) -> [u8; 16] {
        publish_receipt_id(self.key, self.request, head.generation, head.envelope)
    }

    /// What `receipts` say about this key, if any holds it.
    pub fn find(&self, receipts: &[PublishReceiptRecord]) -> Option<Recorded> {
        let receipt = receipts.iter().find(|receipt| receipt.key == self.key)?;
        Some(if receipt.request == self.request {
            Recorded::Published(RepositoryHead {
                generation: receipt.generation,
                envelope: receipt.envelope,
            })
        } else {
            Recorded::Reused
        })
    }

    /// Whether `other` has this key, whatever it asks for.
    pub fn same_key(&self, other: &PublishKey) -> bool {
        self.key == other.key
    }

    /// Whether `other` is this publish sent again.
    pub fn same_request(&self, other: &PublishKey) -> bool {
        self.request == other.request
    }
}

#[cfg(test)]
mod tests {
    use wheat_protocol::{Digest256, Uuid128};

    use super::*;

    fn request(key: u8, envelope: u8) -> PublishRequestOutput {
        PublishRequestOutput {
            message_type: 0x40,
            op_id: 1,
            publish_key: Uuid128 {
                bytes: vec![key; 16],
            },
            envelope_id: Digest256 {
                bytes: vec![envelope; 32],
            },
            expected_generation: 3,
            expected_envelope: Digest256 { bytes: vec![9; 32] },
            force: false,
        }
    }

    fn principal(name: &str) -> Principal {
        Principal {
            name: name.to_string(),
        }
    }

    /// `name`'s publish key for client key `key` and envelope `envelope`.
    fn sent(name: &str, key: u8, envelope: u8) -> PublishKey {
        PublishKey::new(&principal(name), &request(key, envelope)).unwrap()
    }

    fn head() -> RepositoryHead {
        RepositoryHead {
            generation: 4,
            envelope: RepositoryEnvelopeId::from_digest([5; 32]),
        }
    }

    #[test]
    fn a_receipt_answers_its_own_request_and_refuses_another_under_the_same_key() {
        let receipts = [sent("alice", 1, 2).receipt(head(), 100)];
        assert_eq!(
            sent("alice", 1, 2).find(&receipts),
            Some(Recorded::Published(head()))
        );
        assert_eq!(sent("alice", 1, 3).find(&receipts), Some(Recorded::Reused));
        assert_eq!(sent("alice", 2, 2).find(&receipts), None);
    }

    #[test]
    fn the_same_key_from_another_principal_is_another_publish() {
        let receipts = [sent("alice", 1, 2).receipt(head(), 100)];
        assert_eq!(sent("bob", 1, 2).find(&receipts), None);
    }

    #[test]
    fn a_request_with_a_short_id_is_refused() {
        let mut short = request(1, 2);
        short.envelope_id.bytes.pop();
        assert!(matches!(
            PublishKey::new(&principal("alice"), &short),
            Err(ConnectionError::Protocol("invalid envelope ID"))
        ));
    }
}
