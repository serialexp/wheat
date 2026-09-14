use std::collections::BTreeMap;

use crate::{
    CanonicalObjectError, RepositoryEnvelopeId, RepositoryStateId, ViewId, ViewLabelManifestId,
};

/// A native label target and optional provenance from an imported Git ref.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewLabel {
    pub view: ViewId,
    pub original_git_ref: Option<String>,
}

impl From<ViewId> for ViewLabel {
    fn from(view: ViewId) -> Self {
        Self {
            view,
            original_git_ref: None,
        }
    }
}

/// Canonical mapping from native labels to repository views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewLabelManifestRecord {
    pub labels: BTreeMap<String, ViewLabel>,
}

impl ViewLabelManifestRecord {
    /// Builds a manifest, rejecting duplicate or invalid labels and provenance refs.
    pub fn new<I, S, V>(labels: I) -> Result<Self, CanonicalObjectError>
    where
        I: IntoIterator<Item = (S, V)>,
        S: Into<String>,
        V: Into<ViewLabel>,
    {
        let mut canonical = BTreeMap::new();
        for (name, label) in labels {
            let name = name.into();
            validate_label_name(&name)?;
            let label = label.into();
            if let Some(original_git_ref) = &label.original_git_ref {
                validate_original_git_ref(original_git_ref)?;
            }
            if canonical.insert(name, label).is_some() {
                return Err(CanonicalObjectError::InvalidCollection(
                    "duplicate view label manifest entry",
                ));
            }
        }
        Ok(Self { labels: canonical })
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::view_label_manifest_record_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> ViewLabelManifestId {
        ViewLabelManifestId::of_canonical_bytes(&self.canonical_bytes())
    }
}

/// Canonical root object pairing repository state with its named view labels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RepositoryEnvelopeRecord {
    pub state: RepositoryStateId,
    pub labels: ViewLabelManifestId,
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

pub(crate) fn validate_label_name(name: &str) -> Result<(), CanonicalObjectError> {
    if name.is_empty()
        || name.contains('\0')
        || name
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(CanonicalObjectError::InvalidViewLabel(name.to_owned()));
    }
    Ok(())
}

fn validate_original_git_ref(name: &str) -> Result<(), CanonicalObjectError> {
    if !name.starts_with("refs/") || name.contains('\0') {
        return Err(CanonicalObjectError::InvalidViewLabel(name.to_owned()));
    }
    validate_label_name(name)
}
