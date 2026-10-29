use std::{
    fmt,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ChangeId(Uuid);

impl ChangeId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }

    #[must_use]
    pub fn from_import_identity(namespace: Uuid, identity: &[u8]) -> Self {
        Self(Uuid::new_v5(&namespace, identity))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(Uuid::from_bytes(bytes))
    }
}

impl Default for ChangeId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ChangeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for ChangeId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value).map(Self)
    }
}

/// What an object's canonical bytes are hashed after, and a file's content after. The
/// two are prefix-free, so no file's content can have an object's ID, whatever bytes it
/// holds.
const OBJECT_DOMAIN: &[u8] = b"wheat-object\0";
const CONTENT_DOMAIN: &[u8] = b"wheat-content\0";

/// The BLAKE3 hash of `bytes` after `domain`.
fn domain_hash(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

/// Serializes as the hex string an ID displays as, so every JSON document (command
/// output, and the temporary files that are JSON) shows IDs the way the human output
/// does, and reads them back from the same.
macro_rules! hex_serde {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                deserializer.deserialize_str(HexVisitor(std::marker::PhantomData))
            }
        }
    };
}

/// Reads an ID from its hex string without allocating.
struct HexVisitor<T>(std::marker::PhantomData<T>);

impl<T: FromStr<Err = ParseDigestIdError>> de::Visitor<'_> for HexVisitor<T> {
    type Value = T;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an ID of 64 hexadecimal characters")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<T, E> {
        value.parse().map_err(E::custom)
    }
}

macro_rules! digest_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 32]);

        hex_serde!($name);

        impl $name {
            #[must_use]
            pub fn of_canonical_bytes(bytes: &[u8]) -> Self {
                Self(domain_hash(OBJECT_DOMAIN, bytes))
            }

            #[must_use]
            pub fn from_digest(digest: [u8; 32]) -> Self {
                Self(digest)
            }

            #[must_use]
            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = ParseDigestIdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_digest(value).map(Self)
            }
        }
    };
}

digest_id!(ObjectId);
digest_id!(ChangeObjectId);
digest_id!(EditId);
digest_id!(RevisionId);
digest_id!(TreeId);
digest_id!(ApplicationId);
digest_id!(ConflictId);
digest_id!(ViewId);
digest_id!(ContentMapId);
digest_id!(RepositoryStateId);
digest_id!(ViewLabelManifestId);
digest_id!(RepositoryEnvelopeId);
digest_id!(RevisionMapId);
digest_id!(ChangeMapId);
digest_id!(ApplicationMapId);
digest_id!(ConflictSetId);
digest_id!(ContentSetId);
digest_id!(RetainedSetId);
digest_id!(ProjectedCommitSetId);
digest_id!(ChangeSetId);
digest_id!(ViewLineageMapId);
digest_id!(DirectoryChunkId);
digest_id!(HistoryMapId);

/// A member of a [`ProjectedCommitSetId`] set: application `application` projects as
/// the Git SHA-1 commit `commit`. The commit leads, so the members for one commit are
/// adjacent and a lookup by commit is a short range read.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProjectedCommit([u8; ProjectedCommit::LEN]);

impl ProjectedCommit {
    /// Bytes of a Git SHA-1 commit ID.
    pub const COMMIT_LEN: usize = 20;
    /// Bytes of a member: the commit, then the application.
    pub const LEN: usize = Self::COMMIT_LEN + 32;

    #[must_use]
    pub fn new(commit: [u8; Self::COMMIT_LEN], application: ApplicationId) -> Self {
        let mut bytes = [0; Self::LEN];
        bytes[..Self::COMMIT_LEN].copy_from_slice(&commit);
        bytes[Self::COMMIT_LEN..].copy_from_slice(application.as_bytes());
        Self(bytes)
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    #[must_use]
    pub fn commit(&self) -> [u8; Self::COMMIT_LEN] {
        self.0[..Self::COMMIT_LEN]
            .try_into()
            .expect("a member starts with a commit")
    }

    #[must_use]
    pub fn application(&self) -> ApplicationId {
        ApplicationId::from_digest(
            self.0[Self::COMMIT_LEN..]
                .try_into()
                .expect("a member ends with an application"),
        )
    }
}

/// A key of a view's history map ([`HistoryMapId`]): when a change's revision was
/// committed, and the change. Keys order newest first and, within a second, by highest
/// ChangeId first, so a cursor over the map reads the history by time from the newest
/// change on. The bytes are the committed_at seconds with the sign bit flipped,
/// big-endian, then the ChangeId, all inverted.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HistoryKey([u8; HistoryKey::LEN]);

impl HistoryKey {
    /// Bytes of a key: the time, then the change.
    pub const LEN: usize = 8 + 16;

    #[must_use]
    pub fn new(committed_at: i64, change: ChangeId) -> Self {
        let time = committed_at.cast_unsigned() ^ (1 << 63);
        Self(crate::wire::history_key_bytes(time, change).map(|byte| !byte))
    }

    #[must_use]
    pub fn from_bytes(bytes: [u8; Self::LEN]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; Self::LEN] {
        &self.0
    }

    /// The committed_at seconds.
    #[must_use]
    pub fn committed_at(&self) -> i64 {
        (self.parts().0 ^ (1 << 63)).cast_signed()
    }

    #[must_use]
    pub fn change(&self) -> ChangeId {
        self.parts().1
    }

    fn parts(&self) -> (u64, ChangeId) {
        crate::wire::history_key_parts(&self.0.map(|byte| !byte))
    }
}

macro_rules! object_id_from {
    ($($name:ident),+ $(,)?) => {
        $(
            impl From<$name> for ObjectId {
                fn from(value: $name) -> Self {
                    Self::from_digest(*value.as_bytes())
                }
            }
        )+
    };
}

object_id_from!(
    ChangeObjectId,
    EditId,
    RevisionId,
    TreeId,
    ApplicationId,
    ConflictId,
    ViewId,
    ContentMapId,
    RepositoryStateId,
    ViewLabelManifestId,
    RepositoryEnvelopeId,
    RevisionMapId,
    ChangeMapId,
    ApplicationMapId,
    ConflictSetId,
    ContentSetId,
    RetainedSetId,
    ProjectedCommitSetId,
    ChangeSetId,
    ViewLineageMapId,
    DirectoryChunkId,
    HistoryMapId,
);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentId([u8; 32]);

hex_serde!(ContentId);

/// Bytes this process has hashed with [`ContentId::of`].
static CONTENT_BYTES_HASHED: AtomicU64 = AtomicU64::new(0);

/// Bytes this process has hashed into content IDs so far: the work a contract counts
/// to show content is hashed once, not once per layer it passes through.
#[must_use]
pub fn content_bytes_hashed() -> u64 {
    CONTENT_BYTES_HASHED.load(Ordering::Relaxed)
}

impl ContentId {
    #[must_use]
    pub fn of(bytes: &[u8]) -> Self {
        CONTENT_BYTES_HASHED.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Self(domain_hash(CONTENT_DOMAIN, bytes))
    }

    /// A hasher that, fed a file's bytes, finishes with their content ID's digest: for
    /// content read in pieces.
    #[must_use]
    pub fn hasher() -> blake3::Hasher {
        let mut hasher = blake3::Hasher::new();
        hasher.update(CONTENT_DOMAIN);
        hasher
    }

    #[must_use]
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for ContentId {
    type Err = ParseDigestIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_digest(value).map(Self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ParseDigestIdError;

impl fmt::Display for ParseDigestIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("digest ID must be 64 hexadecimal characters")
    }
}

impl std::error::Error for ParseDigestIdError {}

pub type ParseContentIdError = ParseDigestIdError;

fn parse_digest(value: &str) -> Result<[u8; 32], ParseDigestIdError> {
    if value.len() != 64 {
        return Err(ParseDigestIdError);
    }
    let mut bytes = [0_u8; 32];
    for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
        let encoded = std::str::from_utf8(chunk).map_err(|_| ParseDigestIdError)?;
        bytes[index] = u8::from_str_radix(encoded, 16).map_err(|_| ParseDigestIdError)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_as_their_hex_and_read_back() {
        let view = ViewId::from_digest([0xab; 32]);
        let json = serde_json::to_value(view).unwrap();
        assert_eq!(json, serde_json::Value::String(view.to_string()));
        assert_eq!(serde_json::from_value::<ViewId>(json).unwrap(), view);

        let content = ContentId::of(b"bytes");
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, format!("\"{content}\""));
        assert_eq!(serde_json::from_str::<ContentId>(&json).unwrap(), content);

        assert!(serde_json::from_str::<ViewId>("\"not hex\"").is_err());
        assert!(serde_json::from_str::<ViewId>("[1, 2]").is_err());
    }

    #[test]
    fn content_and_objects_of_the_same_bytes_have_different_ids() {
        let bytes = b"the same bytes";
        let content = ContentId::of(bytes);
        let object = ObjectId::of_canonical_bytes(bytes);
        assert_ne!(content.as_bytes(), object.as_bytes());
        assert_ne!(content.as_bytes(), blake3::hash(bytes).as_bytes());
        assert_ne!(object.as_bytes(), blake3::hash(bytes).as_bytes());
    }

    #[test]
    fn the_content_hasher_matches_content_ids() {
        let mut hasher = ContentId::hasher();
        hasher.update(b"in ");
        hasher.update(b"pieces");
        assert_eq!(
            hasher.finalize().as_bytes(),
            ContentId::of(b"in pieces").as_bytes()
        );
    }
}
