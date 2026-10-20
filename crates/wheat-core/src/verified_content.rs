//! Content bytes known to hash to their ID.
//!
//! Content is hashed where its bytes come in: a server receiving an upload, a client
//! receiving a fetch, a workspace file being captured. A [`VerifiedContent`] carries
//! that check on, so the stage and the stores below take it as proof and do not hash
//! the bytes again. Bytes read back from disk or a remote store are hashed by the
//! reader, which is a check of its own.

use std::{borrow::Cow, fmt};

use crate::ContentId;

/// Content bytes and the ID they hash to. Only hashing the bytes makes one.
///
/// It holds its bytes as `B`: owned by default, or borrowed as
/// `VerifiedContent<&[u8]>` (see [`Self::borrowed`]) to pass verified content down
/// without copying it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedContent<B = Vec<u8>> {
    id: ContentId,
    bytes: B,
}

/// Bytes claimed to have an ID they do not hash to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ContentMismatch {
    pub claimed: ContentId,
    pub actual: ContentId,
}

impl fmt::Display for ContentMismatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "content claimed to be {} hashes to {}",
            self.claimed, self.actual
        )
    }
}

impl std::error::Error for ContentMismatch {}

impl<B: AsRef<[u8]>> VerifiedContent<B> {
    /// Hashes `bytes` for their ID.
    #[must_use]
    pub fn new(bytes: B) -> Self {
        Self {
            id: ContentId::of(bytes.as_ref()),
            bytes,
        }
    }

    /// Hashes `bytes` and checks they are the content `claimed`.
    pub fn check(claimed: ContentId, bytes: B) -> Result<Self, ContentMismatch> {
        let verified = Self::new(bytes);
        if verified.id == claimed {
            Ok(verified)
        } else {
            Err(ContentMismatch {
                claimed,
                actual: verified.id,
            })
        }
    }

    #[must_use]
    pub fn id(&self) -> ContentId {
        self.id
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_ref()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes().is_empty()
    }

    /// This content, borrowing its bytes.
    #[must_use]
    pub fn borrowed(&self) -> VerifiedContent<&[u8]> {
        VerifiedContent {
            id: self.id,
            bytes: self.bytes.as_ref(),
        }
    }

    #[must_use]
    pub fn into_inner(self) -> B {
        self.bytes
    }
}

impl<B: AsRef<[u8]> + Into<Vec<u8>>> VerifiedContent<B> {
    /// This content owning its bytes, copied if they were borrowed.
    #[must_use]
    pub fn into_owned(self) -> VerifiedContent {
        VerifiedContent {
            id: self.id,
            bytes: self.bytes.into(),
        }
    }
}

impl<'a> From<VerifiedContent> for VerifiedContent<Cow<'a, [u8]>> {
    fn from(content: VerifiedContent) -> Self {
        Self {
            id: content.id,
            bytes: Cow::Owned(content.bytes),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checking_keeps_bytes_that_hash_to_the_claimed_id_and_refuses_others() {
        let bytes = b"hello wheat".to_vec();
        let id = ContentId::of(&bytes);
        let verified = VerifiedContent::check(id, bytes.clone()).unwrap();
        assert_eq!((verified.id(), verified.bytes()), (id, bytes.as_slice()));
        assert_eq!(verified, VerifiedContent::new(bytes.clone()));
        assert_eq!(verified.borrowed(), VerifiedContent::new(bytes.as_slice()));
        assert_eq!(verified.borrowed().into_owned(), verified);
        let shared: VerifiedContent<Cow<'_, [u8]>> = verified.clone().into();
        assert_eq!((shared.id(), shared.bytes()), (id, bytes.as_slice()));

        let other = ContentId::of(b"other");
        assert_eq!(
            VerifiedContent::check(other, b"hello wheat".to_vec()),
            Err(ContentMismatch {
                claimed: other,
                actual: id
            })
        );
    }
}
