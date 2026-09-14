//! Pure authenticated ownership and bounded union-liveness proofs. No deletion is performed here.
use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;
use wheat_core::{ContentId, ObjectId};

use crate::{InventoryEntry, SnapshotId};

const MAGIC: &[u8; 8] = b"WHEATOWN";
const VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OwnerId(pub [u8; 32]);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum OwnerKind {
    Repository = 1,
    Snapshot = 2,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Owner {
    pub kind: OwnerKind,
    pub id: OwnerId,
}

impl Owner {
    #[must_use]
    pub fn repository(id: [u8; 32]) -> Self {
        Self {
            kind: OwnerKind::Repository,
            id: OwnerId(id),
        }
    }
    #[must_use]
    pub fn snapshot(id: SnapshotId) -> Self {
        Self {
            kind: OwnerKind::Snapshot,
            id: OwnerId(id.0),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnershipManifest {
    pub owner: Owner,
    pub objects: Vec<InventoryEntry<ObjectId>>,
    pub contents: Vec<InventoryEntry<ContentId>>,
}

impl OwnershipManifest {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(51 + (self.objects.len() + self.contents.len()) * 40);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_be_bytes());
        out.push(self.owner.kind as u8);
        out.extend_from_slice(&self.owner.id.0);
        append_inventory(&mut out, &self.objects, |id| id.as_bytes());
        append_inventory(&mut out, &self.contents, |id| id.as_bytes());
        out
    }

    #[must_use]
    pub fn authenticated_id(&self) -> OwnershipManifestId {
        OwnershipManifestId(*blake3::hash(&self.canonical_bytes()).as_bytes())
    }

    fn validate(&self, limits: OwnershipLimits) -> Result<(), OwnershipError> {
        if self.objects.len() > limits.max_objects_per_owner
            || self.contents.len() > limits.max_contents_per_owner
        {
            return Err(OwnershipError::LimitExceeded("owner inventory"));
        }
        validate_inventory(&self.objects)?;
        validate_inventory(&self.contents)
    }
}

fn append_inventory<I>(
    out: &mut Vec<u8>,
    inventory: &[InventoryEntry<I>],
    digest: impl Fn(&I) -> &[u8; 32],
) {
    out.extend_from_slice(&(inventory.len() as u64).to_be_bytes());
    for entry in inventory {
        out.extend_from_slice(digest(&entry.id));
        out.extend_from_slice(&entry.bytes.to_be_bytes());
    }
}

fn validate_inventory<I: Ord>(entries: &[InventoryEntry<I>]) -> Result<(), OwnershipError> {
    if entries.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return Err(OwnershipError::InvalidInventory);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OwnershipManifestId(pub [u8; 32]);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OwnershipLimits {
    pub max_owners: usize,
    pub max_objects_per_owner: usize,
    pub max_contents_per_owner: usize,
    pub max_live_objects: usize,
    pub max_live_contents: usize,
}

impl Default for OwnershipLimits {
    fn default() -> Self {
        Self {
            max_owners: 100_000,
            max_objects_per_owner: 1_000_000,
            max_contents_per_owner: 1_000_000,
            max_live_objects: 10_000_000,
            max_live_contents: 10_000_000,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LivenessProof {
    pub objects: BTreeSet<ObjectId>,
    pub contents: BTreeSet<ContentId>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum OwnershipError {
    #[error("ownership manifest authentication failed")]
    Authentication,
    #[error("ownership inventory is not exact canonical order")]
    InvalidInventory,
    #[error("owner has ambiguous authenticated manifests")]
    AmbiguousOwner,
    #[error("ownership proof limit exceeded: {0}")]
    LimitExceeded(&'static str),
    #[error("liveness consumer rejected the proof")]
    Consumer,
}

/// Validates authorities and emits sorted inventory runs without constructing a repository-sized
/// union. A consumer can externally merge the runs or mark liveness incrementally. Duplicate
/// owner manifests are emitted once. `max_live_objects` and `max_live_contents` are also hard
/// aggregate emitted-entry work bounds: entries duplicated across distinct owners count each time.
/// This keeps callback work bounded without claiming to deduplicate a repository-sized union in
/// memory. No callback runs unless all authorities and aggregate work first validate.
pub fn prove_union_liveness_with<'a, FObject, FContent>(
    manifests: impl IntoIterator<Item = (&'a OwnershipManifest, OwnershipManifestId)>,
    limits: OwnershipLimits,
    mut objects: FObject,
    mut contents: FContent,
) -> Result<(), OwnershipError>
where
    FObject: FnMut(&[InventoryEntry<ObjectId>]) -> Result<(), OwnershipError>,
    FContent: FnMut(&[InventoryEntry<ContentId>]) -> Result<(), OwnershipError>,
{
    let mut authorities = BTreeMap::new();
    let mut accepted = Vec::new();
    let mut emitted_objects = 0usize;
    let mut emitted_contents = 0usize;
    for (manifest, claimed) in manifests {
        if authorities.len() >= limits.max_owners && !authorities.contains_key(&manifest.owner) {
            return Err(OwnershipError::LimitExceeded("owners"));
        }
        manifest.validate(limits)?;
        if manifest.authenticated_id() != claimed {
            return Err(OwnershipError::Authentication);
        }
        match authorities.insert(manifest.owner, claimed) {
            Some(previous) if previous != claimed => return Err(OwnershipError::AmbiguousOwner),
            Some(_) => {}
            None => {
                emitted_objects = emitted_objects
                    .checked_add(manifest.objects.len())
                    .ok_or(OwnershipError::LimitExceeded("live objects"))?;
                emitted_contents = emitted_contents
                    .checked_add(manifest.contents.len())
                    .ok_or(OwnershipError::LimitExceeded("live contents"))?;
                if emitted_objects > limits.max_live_objects {
                    return Err(OwnershipError::LimitExceeded("live objects"));
                }
                if emitted_contents > limits.max_live_contents {
                    return Err(OwnershipError::LimitExceeded("live contents"));
                }
                accepted.push(manifest);
            }
        }
    }
    for manifest in accepted {
        objects(&manifest.objects)?;
        contents(&manifest.contents)?;
    }
    Ok(())
}

/// Convenience in-memory union adapter. Use [`prove_union_liveness_with`] for production-scale
/// marking so retained IDs do not need to be accumulated in RAM.
pub fn prove_union_liveness<'a>(
    manifests: impl IntoIterator<Item = (&'a OwnershipManifest, OwnershipManifestId)>,
) -> Result<LivenessProof, OwnershipError> {
    prove_union_liveness_bounded(manifests, OwnershipLimits::default())
}

pub fn prove_union_liveness_bounded<'a>(
    manifests: impl IntoIterator<Item = (&'a OwnershipManifest, OwnershipManifestId)>,
    limits: OwnershipLimits,
) -> Result<LivenessProof, OwnershipError> {
    let mut proof = LivenessProof::default();
    prove_union_liveness_with(
        manifests,
        limits,
        |entries| {
            for entry in entries {
                proof.objects.insert(entry.id);
                if proof.objects.len() > limits.max_live_objects {
                    return Err(OwnershipError::LimitExceeded("live objects"));
                }
            }
            Ok(())
        },
        |entries| {
            for entry in entries {
                proof.contents.insert(entry.id);
                if proof.contents.len() > limits.max_live_contents {
                    return Err(OwnershipError::LimitExceeded("live contents"));
                }
            }
            Ok(())
        },
    )?;
    Ok(proof)
}
