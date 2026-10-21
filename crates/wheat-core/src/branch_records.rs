use std::collections::BTreeMap;

use crate::{
    Actor, CanonicalObjectError, RepositoryEnvelopeId, RepositoryStateId, Timestamp, ViewId,
    ViewLabelManifestId,
};

/// Largest annotation message a label may carry. The whole label manifest is
/// rewritten on every label change, so tag messages stay far below revision messages.
pub const MAX_LABEL_ANNOTATION_MESSAGE_BYTES: usize = 64 * 1024;

/// A native label target, optional provenance from an imported Git ref, and an
/// optional annotation that makes it publishable as an annotated Git tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewLabel {
    pub view: ViewId,
    pub original_git_ref: Option<String>,
    pub annotation: Option<LabelAnnotation>,
}

/// Who tagged a label, when, and why. It is stored rather than made at push time,
/// so the Git tag object built from it has the same ID wherever it is pushed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LabelAnnotation {
    pub tagger: Actor,
    pub tagged_at: Timestamp,
    pub message: String,
}

impl LabelAnnotation {
    /// Checks the fields a Git tag object can carry: a tagger name and email
    /// without `<`, `>`, newlines or NULs, and a nonempty message without NULs
    /// of at most [`MAX_LABEL_ANNOTATION_MESSAGE_BYTES`].
    pub fn validate(&self) -> Result<(), CanonicalObjectError> {
        let identity_ok =
            |value: &str| !value.is_empty() && !value.contains(['<', '>', '\n', '\r', '\0']);
        if !identity_ok(&self.tagger.name) || !identity_ok(&self.tagger.email) {
            return Err(CanonicalObjectError::InvalidLabelAnnotation(
                "tagger name and email must be nonempty and contain no <, >, newline or NUL",
            ));
        }
        if self.message.trim().is_empty() {
            return Err(CanonicalObjectError::InvalidLabelAnnotation(
                "message must not be empty",
            ));
        }
        if self.message.contains('\0') {
            return Err(CanonicalObjectError::InvalidLabelAnnotation(
                "message must not contain NUL",
            ));
        }
        if self.message.len() > MAX_LABEL_ANNOTATION_MESSAGE_BYTES {
            return Err(CanonicalObjectError::InvalidLabelAnnotation(
                "message is longer than 64 KiB",
            ));
        }
        Ok(())
    }
}

impl From<ViewId> for ViewLabel {
    fn from(view: ViewId) -> Self {
        Self {
            view,
            original_git_ref: None,
            annotation: None,
        }
    }
}

/// The most labels a repository holds. A manifest with more is refused when it is
/// built or decoded, so whoever receives one can read it whole.
pub const MAX_LABELS: usize = 10_000;

/// Canonical mapping from native labels to repository views.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewLabelManifestRecord {
    pub labels: BTreeMap<String, ViewLabel>,
}

impl ViewLabelManifestRecord {
    /// Builds a manifest, rejecting duplicate or invalid labels and provenance refs, and
    /// more than [`MAX_LABELS`] labels.
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
            if let Some(annotation) = &label.annotation {
                annotation.validate()?;
            }
            if canonical.insert(name, label).is_some() {
                return Err(CanonicalObjectError::InvalidCollection(
                    "duplicate view label manifest entry",
                ));
            }
            if canonical.len() > MAX_LABELS {
                return Err(CanonicalObjectError::InvalidCollection(
                    "view label manifest has more labels than a repository may hold",
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
