use std::collections::BTreeMap;

use crate::{
    BranchManifestId, CanonicalObjectError, RepositoryEnvelopeId, RepositoryStateId, ViewId,
};

/// Canonical mapping from branch names to repository views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BranchManifestRecord {
    pub branches: BTreeMap<String, ViewId>,
}

impl BranchManifestRecord {
    /// Builds a manifest, rejecting duplicate or invalid branch names.
    pub fn new<I, S>(branches: I) -> Result<Self, CanonicalObjectError>
    where
        I: IntoIterator<Item = (S, ViewId)>,
        S: Into<String>,
    {
        let mut canonical = BTreeMap::new();
        for (name, view) in branches {
            let name = name.into();
            validate_branch_name(&name)?;
            if canonical.insert(name, view).is_some() {
                return Err(CanonicalObjectError::InvalidCollection(
                    "duplicate branch manifest entry",
                ));
            }
        }
        Ok(Self {
            branches: canonical,
        })
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::branch_manifest_record_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> BranchManifestId {
        BranchManifestId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// Canonical root object pairing repository state with its named branches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryEnvelopeRecord {
    pub state: RepositoryStateId,
    pub branches: BranchManifestId,
}

impl RepositoryEnvelopeRecord {
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::repository_envelope_record_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> RepositoryEnvelopeId {
        RepositoryEnvelopeId::of_canonical_bytes(&self.canonical_bytes())
    }
}

pub(crate) fn validate_branch_name(name: &str) -> Result<(), CanonicalObjectError> {
    if name.is_empty()
        || name.contains('\0')
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(CanonicalObjectError::InvalidBranchName(name.to_owned()));
    }
    Ok(())
}
