//! Pure application adapter from persisted edit records to a flat tree.

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
};

use crate::{
    CanonicalObjectError, ContentId, EditId, EditOperation, EditRecord, FileKind, FileValue, Tree,
    guarded,
};

/// Default bound on the edits one revision may apply.
///
/// Application holds one record per edit plus the target values of the paths they touch, and
/// resolves content one path group at a time, so its cost grows with the edit count rather than
/// the repository. The bound matches the Merkle tree update bound that installs the result; the
/// per-file bound on guarded hunks is separate ([`guarded::Limits::max_edits`]).
pub const DEFAULT_MAX_REVISION_EDITS: usize = 250_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationLimits {
    /// Maximum edits in one revision, across every path.
    pub max_edits: usize,
    /// Maximum content bytes held while applying one path group.
    pub max_resolved_bytes: usize,
    pub max_generated_bytes: usize,
    /// Accept edits whose intended replacement is already present. This is for
    /// descendant reapplication only; authored revisions remain strict.
    pub allow_already_satisfied: bool,
    pub guarded: guarded::Limits,
}

impl Default for ApplicationLimits {
    fn default() -> Self {
        Self {
            max_edits: DEFAULT_MAX_REVISION_EDITS,
            max_resolved_bytes: 64 * 1024 * 1024,
            max_generated_bytes: 16 * 1024 * 1024,
            allow_already_satisfied: false,
            guarded: guarded::Limits::default(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationOutcome {
    Applied {
        read: Option<Range<usize>>,
        write: Option<Range<usize>>,
    },
    AlreadySatisfied {
        read: Option<Range<usize>>,
    },
    NoMatch,
    Ambiguous,
    Interference {
        with: Vec<EditId>,
    },
    KindMismatch {
        expected: FileKind,
        actual: Option<FileKind>,
    },
    ExpectedFileMismatch {
        expected: Option<FileValue>,
        actual: Option<FileValue>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationResult {
    pub tree: Tree,
    /// New blobs produced by guarded text edits. Exact replacements remain externally addressed.
    pub generated_content: BTreeMap<ContentId, Vec<u8>>,
    pub outcomes: BTreeMap<EditId, ApplicationOutcome>,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApplicationError {
    TooManyEdits {
        actual: usize,
        limit: usize,
    },
    ByteLimitExceeded {
        generated: bool,
        actual: usize,
        limit: usize,
    },
    DuplicateEditId(EditId),
    InvalidEdit {
        id: EditId,
        source: CanonicalObjectError,
    },
    EditIdMismatch {
        supplied: EditId,
        actual: EditId,
    },
    Resolve {
        id: ContentId,
        message: String,
    },
    CorruptContent {
        expected: ContentId,
        actual: ContentId,
    },
    Guarded(guarded::Error),
}

/// Applies `edits` deterministically. Every definition is matched against the immutable target.
pub fn apply_edits<E>(
    target: &Tree,
    edit_iter: impl IntoIterator<Item = (EditId, EditRecord)>,
    mut resolve: impl FnMut(ContentId) -> Result<Vec<u8>, E>,
    limits: ApplicationLimits,
) -> Result<ApplicationResult, ApplicationError>
where
    E: std::fmt::Display,
{
    let mut edits: Vec<(EditId, EditRecord)> = Vec::new();
    for edit in edit_iter {
        if edits.len() == limits.max_edits {
            return Err(ApplicationError::TooManyEdits {
                actual: edits.len().saturating_add(1),
                limit: limits.max_edits,
            });
        }
        edits.push(edit);
    }
    // Validate in canonical order as well: failures must not depend on caller iteration order.
    edits.sort_by_key(|(id, _)| *id);
    let mut ids = BTreeSet::new();
    for (id, record) in &edits {
        if !ids.insert(*id) {
            return Err(ApplicationError::DuplicateEditId(*id));
        }
        EditRecord::new(record.path.clone(), record.operation.clone())
            .map_err(|source| ApplicationError::InvalidEdit { id: *id, source })?;
        let actual = record.id();
        if actual != *id {
            return Err(ApplicationError::EditIdMismatch {
                supplied: *id,
                actual,
            });
        }
    }

    // `limits.guarded.max_edits` bounds the hunks applied to one file; each path group below
    // enforces it. The revision as a whole is bounded by `limits.max_edits` above.
    let mut tree = target.clone();
    let mut outcomes = BTreeMap::new();
    let mut generated_content = BTreeMap::new();
    let mut generated_total = 0usize;
    let mut paths = BTreeMap::<String, Vec<(EditId, EditRecord)>>::new();
    for edit in edits {
        paths.entry(edit.1.path.clone()).or_default().push(edit);
    }
    // Every path charges the same aggregate budget; cheap paths leave capacity for expensive ones.
    let mut guarded_work = guarded::WorkBudget::new(limits.guarded.max_work);

    for (path, group) in paths {
        // Resolve and authenticate one path group's content, then drop it before advancing.
        // Definitions are resolved even when kind/precondition checks will fail so missing or
        // corrupt records remain fatal independently of the target being applied to.
        let mut needed = BTreeSet::new();
        if let Some(value) = target.get(&path) {
            needed.insert(value.content);
        }
        for (_, record) in &group {
            match &record.operation {
                EditOperation::GuardedText {
                    before,
                    expected,
                    replacement,
                    after,
                    ..
                } => needed.extend([*before, *expected, *replacement, *after]),
                EditOperation::ExactFile {
                    expected,
                    replacement,
                } => needed.extend(
                    expected
                        .iter()
                        .chain(replacement.iter())
                        .map(|value| value.content),
                ),
            }
        }
        let mut cache = BTreeMap::<ContentId, Vec<u8>>::new();
        let mut resolved_total = 0usize;
        for id in needed {
            let value = resolve(id).map_err(|error| ApplicationError::Resolve {
                id,
                message: error.to_string(),
            })?;
            let actual = ContentId::of(&value);
            if actual != id {
                return Err(ApplicationError::CorruptContent {
                    expected: id,
                    actual,
                });
            }
            resolved_total = resolved_total.checked_add(value.len()).ok_or(
                ApplicationError::ByteLimitExceeded {
                    generated: false,
                    actual: usize::MAX,
                    limit: limits.max_resolved_bytes,
                },
            )?;
            if resolved_total > limits.max_resolved_bytes {
                return Err(ApplicationError::ByteLimitExceeded {
                    generated: false,
                    actual: resolved_total,
                    limit: limits.max_resolved_bytes,
                });
            }
            cache.insert(id, value);
        }

        if group.len() > 1
            && group
                .iter()
                .any(|(_, record)| matches!(record.operation, EditOperation::ExactFile { .. }))
        {
            let all: Vec<_> = group.iter().map(|(id, _)| *id).collect();
            for (id, _) in group {
                outcomes.insert(
                    id,
                    ApplicationOutcome::Interference {
                        with: all.iter().copied().filter(|other| other != &id).collect(),
                    },
                );
            }
            continue;
        }
        if let [(id, record)] = group.as_slice()
            && let EditOperation::ExactFile {
                expected,
                replacement,
            } = &record.operation
        {
            let actual = target.get(&path).copied();
            if limits.allow_already_satisfied && actual == *replacement {
                outcomes.insert(*id, ApplicationOutcome::AlreadySatisfied { read: None });
            } else if actual != *expected {
                outcomes.insert(
                    *id,
                    ApplicationOutcome::ExpectedFileMismatch {
                        expected: *expected,
                        actual,
                    },
                );
            } else {
                match replacement {
                    Some(value) => {
                        tree.insert(path, *value);
                    }
                    None => {
                        tree.remove(&path);
                    }
                }
                outcomes.insert(
                    *id,
                    ApplicationOutcome::Applied {
                        read: None,
                        write: None,
                    },
                );
            }
            continue;
        }

        let actual = target.get(&path).copied();
        let mut applicable = Vec::new();
        for (id, record) in &group {
            let EditOperation::GuardedText {
                kind,
                before,
                expected,
                replacement,
                after,
                start_anchor,
                end_anchor,
            } = &record.operation
            else {
                unreachable!()
            };
            if actual.map(|value| value.kind) != Some(*kind) {
                outcomes.insert(
                    *id,
                    ApplicationOutcome::KindMismatch {
                        expected: *kind,
                        actual: actual.map(|value| value.kind),
                    },
                );
                continue;
            }
            applicable.push((
                *id,
                guarded::GuardedEditRef {
                    before: &cache[before],
                    expected: &cache[expected],
                    replacement: &cache[replacement],
                    after: &cache[after],
                    start_anchor: *start_anchor,
                    end_anchor: *end_anchor,
                },
            ));
        }
        if applicable.is_empty() {
            continue;
        }
        let source = &cache[&actual.expect("kind checked").content];
        let guarded_edits: Vec<_> = applicable.iter().map(|(_, edit)| *edit).collect();
        let remaining_generated = limits.max_generated_bytes.saturating_sub(generated_total);
        let result = guarded::ApplyResult::apply_borrowed(
            source,
            &guarded_edits,
            limits.guarded,
            remaining_generated,
            &mut guarded_work,
            limits.allow_already_satisfied,
        )
        .map_err(|error| match error {
            guarded::Error::OutputTooLarge { actual, limit: _ } => {
                ApplicationError::ByteLimitExceeded {
                    generated: true,
                    actual: generated_total.saturating_add(actual),
                    limit: limits.max_generated_bytes,
                }
            }
            error => ApplicationError::Guarded(error),
        })?;
        for ((id, _), outcome) in applicable.iter().zip(&result.outcomes) {
            let mapped = match outcome {
                guarded::EditOutcome::Applied { read, write } => ApplicationOutcome::Applied {
                    read: Some(read.clone()),
                    write: Some(write.clone()),
                },
                guarded::EditOutcome::AlreadySatisfied { read } => {
                    ApplicationOutcome::AlreadySatisfied {
                        read: Some(read.clone()),
                    }
                }
                guarded::EditOutcome::NoMatch => ApplicationOutcome::NoMatch,
                guarded::EditOutcome::Ambiguous => ApplicationOutcome::Ambiguous,
                guarded::EditOutcome::Interference { with } => ApplicationOutcome::Interference {
                    with: with.iter().map(|index| applicable[*index].0).collect(),
                },
            };
            outcomes.insert(*id, mapped);
        }
        if result.content != *source {
            generated_total = generated_total.checked_add(result.content.len()).ok_or(
                ApplicationError::ByteLimitExceeded {
                    generated: true,
                    actual: usize::MAX,
                    limit: limits.max_generated_bytes,
                },
            )?;
            if generated_total > limits.max_generated_bytes {
                return Err(ApplicationError::ByteLimitExceeded {
                    generated: true,
                    actual: generated_total,
                    limit: limits.max_generated_bytes,
                });
            }
            let content = ContentId::of(&result.content);
            generated_content.entry(content).or_insert(result.content);
            tree.insert(
                path,
                FileValue {
                    content,
                    kind: actual.expect("kind checked").kind,
                },
            );
        }
    }
    Ok(ApplicationResult {
        tree,
        generated_content,
        outcomes,
    })
}
