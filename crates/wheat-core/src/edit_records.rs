use serde::{Deserialize, Serialize};

use crate::{CanonicalObjectError, ContentId, EditId, FileKind, FileValue};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EditRecord {
    pub path: String,
    pub operation: EditOperation,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EditOperation {
    GuardedText {
        kind: FileKind,
        before: ContentId,
        expected: ContentId,
        replacement: ContentId,
        after: ContentId,
        start_anchor: bool,
        end_anchor: bool,
    },
    ExactFile {
        expected: Option<FileValue>,
        replacement: Option<FileValue>,
    },
}

impl EditRecord {
    pub fn new(
        path: impl Into<String>,
        operation: EditOperation,
    ) -> Result<Self, CanonicalObjectError> {
        if let EditOperation::GuardedText {
            expected,
            replacement,
            ..
        } = &operation
            && expected == replacement
        {
            return Err(CanonicalObjectError::InvalidCollection(
                "guarded text edit must not be a no-op",
            ));
        }
        let record = Self {
            path: path.into(),
            operation,
        };
        crate::wire::validate_edit_record(&record)?;
        Ok(record)
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::wire::edit_record_bytes(self)
    }

    #[must_use]
    pub fn id(&self) -> EditId {
        EditId::of_canonical_bytes(&self.canonical_bytes())
    }
}

pub fn decode_edit_record(bytes: &[u8]) -> Result<EditRecord, CanonicalObjectError> {
    crate::wire::decode_edit_record(bytes)
}
