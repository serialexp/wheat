use std::{
    fmt,
    str::FromStr,
    sync::atomic::{AtomicU64, Ordering},
};

use serde::{Deserialize, Serialize};
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

macro_rules! digest_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
        )]
        pub struct $name([u8; 32]);

        impl $name {
            #[must_use]
            pub fn of_canonical_bytes(bytes: &[u8]) -> Self {
                Self(*blake3::hash(bytes).as_bytes())
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
);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ContentId([u8; 32]);

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
        Self(*blake3::hash(bytes).as_bytes())
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
