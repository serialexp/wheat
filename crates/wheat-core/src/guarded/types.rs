use std::ops::Range;

/// Conservative defaults keep hostile inputs from causing unbounded scans.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_EDITS: usize = 4096;
pub const DEFAULT_MAX_WORK: usize = 64 * 1024 * 1024;
pub const DEFAULT_EXTRA_CONTEXT_LINES: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardedEdit {
    pub before: Vec<u8>,
    pub expected: Vec<u8>,
    pub replacement: Vec<u8>,
    pub after: Vec<u8>,
    pub start_anchor: bool,
    pub end_anchor: bool,
}

impl GuardedEdit {
    pub fn read_len(&self) -> Option<usize> {
        self.before
            .len()
            .checked_add(self.expected.len())?
            .checked_add(self.after.len())
    }
}

/// Borrowed edit definition used by aggregate application to avoid copying resolved blobs.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GuardedEditRef<'a> {
    pub before: &'a [u8],
    pub expected: &'a [u8],
    pub replacement: &'a [u8],
    pub after: &'a [u8],
    pub start_anchor: bool,
    pub end_anchor: bool,
}

impl<'a> From<&'a GuardedEdit> for GuardedEditRef<'a> {
    fn from(edit: &'a GuardedEdit) -> Self {
        Self {
            before: &edit.before,
            expected: &edit.expected,
            replacement: &edit.replacement,
            after: &edit.after,
            start_anchor: edit.start_anchor,
            end_anchor: edit.end_anchor,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkBudget {
    used: usize,
    limit: usize,
}

impl WorkBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    pub(crate) fn with_used(limit: usize, used: usize) -> Self {
        Self { used, limit }
    }

    pub(crate) fn charge(&mut self, amount: usize) -> Result<(), Error> {
        self.used = self
            .used
            .checked_add(amount)
            .ok_or(Error::WorkBudgetExceeded { limit: self.limit })?;
        if self.used > self.limit {
            return Err(Error::WorkBudgetExceeded { limit: self.limit });
        }
        Ok(())
    }

    pub(crate) fn remaining(self) -> usize {
        self.limit.saturating_sub(self.used)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_edits: usize,
    /// Maximum candidate byte comparisons across matching/capture.
    pub max_work: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_edits: DEFAULT_MAX_EDITS,
            max_work: DEFAULT_MAX_WORK,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    InputTooLarge {
        actual: usize,
        limit: usize,
    },
    OutputTooLarge {
        actual: usize,
        limit: usize,
    },
    TooManyEdits {
        actual: usize,
        limit: usize,
    },
    WorkBudgetExceeded {
        limit: usize,
    },
    NonUtf8,
    NonCanonicalLineEnding,
    NulByte,
    InvalidRange,
    NotUtf8Boundary {
        offset: usize,
    },
    ExplicitGuardDoesNotContainReplacement,
    /// The source itself cannot provide a unique guard, even at full-file scope.
    CaptureAmbiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MatchStatus {
    Unique(MatchedGuard),
    NoMatch,
    Ambiguous,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchedGuard {
    pub read: Range<usize>,
    pub write: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Applied {
        read: Range<usize>,
        write: Range<usize>,
    },
    AlreadySatisfied {
        read: Range<usize>,
    },
    NoMatch,
    Ambiguous,
    Interference {
        with: Vec<usize>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyResult {
    pub content: Vec<u8>,
    pub outcomes: Vec<EditOutcome>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureRequest {
    pub replacement: Range<usize>,
    /// Optional authored guard range. Its edges are lower bounds on context;
    /// uniqueness expansion may grow beyond it.
    pub wider_guard: Option<Range<usize>>,
}

pub(crate) fn validate_text(bytes: &[u8], limits: Limits) -> Result<(), Error> {
    if bytes.len() > limits.max_input_bytes {
        return Err(Error::InputTooLarge {
            actual: bytes.len(),
            limit: limits.max_input_bytes,
        });
    }
    if bytes.contains(&0) {
        return Err(Error::NulByte);
    }
    std::str::from_utf8(bytes).map_err(|_| Error::NonUtf8)?;
    if bytes.contains(&b'\r') {
        return Err(Error::NonCanonicalLineEnding);
    }
    Ok(())
}

pub(crate) fn validate_fragment(bytes: &[u8], limits: Limits) -> Result<(), Error> {
    validate_text(bytes, limits)
}

pub(crate) fn boundary(bytes: &[u8], offset: usize) -> Result<(), Error> {
    if offset > bytes.len() {
        return Err(Error::InvalidRange);
    }
    if !std::str::from_utf8(bytes)
        .expect("validated text")
        .is_char_boundary(offset)
    {
        return Err(Error::NotUtf8Boundary { offset });
    }
    Ok(())
}
