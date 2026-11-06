//! Which change last wrote each line of a file in a view (see
//! `docs/design/wheat-blame.md`). The walk takes the file's touching changes newest
//! first from the [`PathHistoryIndex`] and diffs each one's version against the one
//! before it, so it costs what the file's own history does, not the view's.

use std::num::NonZeroUsize;

use imara_diff::{Algorithm, Diff, InternedInput};
use thiserror::Error;
use wheat_core::{ApplicationRecord, ContentId, FileValue, ViewId, ViewRecord, text_capture};

use crate::{
    ContentStoreError, ImmutableObjectStore, MerkleTreeError, MerkleTreeStore, PathHistoryIndex,
    PathHistoryIndexError, PathTouch, RepositoryPublicationError, RepositoryReader,
};

/// The most touching changes one page of the walk asks the index for.
const TOUCH_PAGE: usize = 64;

#[derive(Debug, Error)]
pub enum BlameError {
    #[error("path {0:?} has an empty component")]
    InvalidPath(String),
    #[error("the file is not text")]
    NotText,
    #[error("the file is {size} bytes, more than the {max} a blame reads")]
    TooLong { size: u64, max: usize },
    #[error("repository record {0} does not match its identity")]
    IdentityMismatch(&'static str),
    #[error(transparent)]
    Read(#[from] RepositoryPublicationError),
    #[error(transparent)]
    Tree(#[from] MerkleTreeError),
    #[error(transparent)]
    Content(#[from] ContentStoreError),
    #[error(transparent)]
    PathHistory(#[from] PathHistoryIndexError),
}

/// How far one blame goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlameLimits {
    /// The longest version of the file read. The file at the view must fit; an
    /// older version that does not ends the walk, its lines left to the change that
    /// shortened it.
    pub max_bytes: usize,
    /// The most touching changes walked.
    pub max_changes: NonZeroUsize,
}

/// Who wrote a range of lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlameOrigin {
    /// The change at this index of the blame's origins.
    Change { index: usize },
    /// Older than every change the walk reached: these lines are lines
    /// `line..line + len` of the file in view `before`, where a blame of the same
    /// path can carry on.
    Unresolved { before: ViewId, line: u32 },
}

/// Lines `start..start + len` of the file and who wrote them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlameRange {
    pub start: u32,
    pub len: u32,
    pub origin: BlameOrigin,
}

/// A file's lines by the change that last wrote them; the origins are `T`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Blame<T> {
    /// The file's lines at the view. A line ends after its `\n`; a last line without
    /// one counts too.
    pub lines: u32,
    /// In line order, covering every line once; neighbours have different origins.
    pub ranges: Vec<BlameRange>,
    /// The changes the ranges name, newest first.
    pub origins: Vec<T>,
}

/// A change that wrote lines of the file, as the walk read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlameChange {
    pub touch: PathTouch,
    pub application: ApplicationRecord,
}

/// A line not yet attributed: its line at the view and in the version being walked.
#[derive(Clone, Copy)]
struct Pending {
    line: u32,
    at: u32,
}

/// A version of the file the walk reads.
enum Version {
    Text(Vec<u8>),
    /// Longer than the walk reads.
    Unreadable {
        size: u64,
    },
    Binary,
}

/// The blame of the file at `path` in `view`, or `None` when no file is there.
///
/// Each touching change walked costs a few tree node reads in its parent's tree,
/// its application and its parent view, one read of the file's version before it
/// (each version is read once), and a line diff (histogram, by `imara-diff`).
#[tracing::instrument(name = "blame", level = "debug", skip_all)]
pub async fn blame<O: ImmutableObjectStore>(
    reader: &RepositoryReader<'_, O>,
    index: &PathHistoryIndex,
    view: &ViewRecord,
    path: &str,
    limits: BlameLimits,
) -> Result<Option<Blame<BlameChange>>, BlameError> {
    let components = components(path)?;
    if components.is_empty() {
        return Ok(None);
    }
    let trees = MerkleTreeStore::new(reader.objects);
    let Some(value) = trees.lookup(view.tree, &components).await? else {
        return Ok(None);
    };
    let mut after = match version(reader, value.content, limits.max_bytes)? {
        Version::Text(bytes) => bytes,
        Version::Binary => return Err(BlameError::NotText),
        Version::Unreadable { size } => {
            return Err(BlameError::TooLong {
                size,
                max: limits.max_bytes,
            });
        }
    };
    let mut after_content = value.content;
    let lines = u32::try_from(line_count(&after)).map_err(|_| BlameError::TooLong {
        size: u64::try_from(after.len()).unwrap_or(u64::MAX),
        max: limits.max_bytes,
    })?;
    // Each line's origin, `UNRESOLVED` until the walk finds it.
    let mut origin = vec![UNRESOLVED; lines as usize];
    let mut pending: Vec<Pending> = (0..lines).map(|line| Pending { line, at: line }).collect();
    let mut still_pending = Vec::with_capacity(pending.len());
    let mut origins = Vec::new();
    let mut diff = Diff::default();
    let mut walked = 0usize;
    let mut before_position = None;
    // Where unresolved lines are, in the version before the oldest change walked.
    let mut oldest = view.id();
    'walk: while !pending.is_empty() && walked < limits.max_changes.get() {
        let page = (limits.max_changes.get() - walked).min(TOUCH_PAGE);
        let touches = index
            .touching(
                reader,
                view,
                path,
                before_position,
                NonZeroUsize::new(page).expect("a page of the walk is not empty"),
            )
            .await?;
        for touch in &touches.touches {
            walked += 1;
            let application = reader.application(touch.application).await?;
            if application.id() != touch.application || application.order.position != touch.position
            {
                return Err(BlameError::IdentityMismatch("application"));
            }
            let parent = reader.view(application.target_view).await?;
            if parent.id() != application.target_view {
                return Err(BlameError::IdentityMismatch("view"));
            }
            oldest = application.target_view;
            let before = trees.lookup(parent.tree, &components).await?;
            let attributed = match before {
                Some(FileValue { content, .. }) if content == after_content => false,
                Some(FileValue { content, .. }) => {
                    match version(reader, content, limits.max_bytes)? {
                        Version::Text(before) => {
                            let attributed = attribute(
                                &mut diff,
                                &before,
                                &after,
                                &mut pending,
                                &mut still_pending,
                                &mut origin,
                                origins.len(),
                            );
                            after = before;
                            after_content = content;
                            attributed
                        }
                        Version::Binary | Version::Unreadable { .. } => {
                            attribute_all(&mut pending, &mut origin, origins.len())
                        }
                    }
                }
                None => attribute_all(&mut pending, &mut origin, origins.len()),
            };
            if attributed {
                origins.push(BlameChange {
                    touch: *touch,
                    application,
                });
            }
            if pending.is_empty() {
                break 'walk;
            }
        }
        match touches.next {
            Some(next) => before_position = Some(next),
            None => break,
        }
    }
    Ok(Some(Blame {
        lines,
        ranges: ranges(&origin, &pending, oldest),
        origins,
    }))
}

const UNRESOLVED: u32 = u32::MAX;

/// Attributes the pending lines inside `after`'s added hunks to origin `index` and
/// moves the rest to their line in `before`; whether it attributed any.
fn attribute(
    diff: &mut Diff,
    before: &[u8],
    after: &[u8],
    pending: &mut Vec<Pending>,
    still_pending: &mut Vec<Pending>,
    origin: &mut [u32],
    index: usize,
) -> bool {
    let input = InternedInput::new(before, after);
    diff.compute_with(
        Algorithm::Histogram,
        &input.before,
        &input.after,
        input.interner.num_tokens(),
    );
    diff.postprocess_lines(&input);
    let index = u32::try_from(index).expect("a blame's origins fit its lines");
    let mut attributed = false;
    let mut hunks = diff.hunks().peekable();
    // A line outside every hunk is at `at + offset` in `before`.
    let mut offset = 0i64;
    still_pending.clear();
    for line in pending.iter() {
        while let Some(hunk) = hunks.peek() {
            // A hunk ending at or before the line (a removal just above it included)
            // is behind it.
            if hunk.after.end > line.at {
                break;
            }
            offset = i64::from(hunk.before.end) - i64::from(hunk.after.end);
            hunks.next();
        }
        if hunks
            .peek()
            .is_some_and(|hunk| hunk.after.contains(&line.at))
        {
            origin[line.line as usize] = index;
            attributed = true;
        } else {
            let at = u32::try_from(i64::from(line.at) + offset)
                .expect("an unchanged line is a line of the version before");
            still_pending.push(Pending {
                line: line.line,
                at,
            });
        }
    }
    std::mem::swap(pending, still_pending);
    attributed
}

/// Attributes every pending line to origin `index`; whether there were any.
fn attribute_all(pending: &mut Vec<Pending>, origin: &mut [u32], index: usize) -> bool {
    let index = u32::try_from(index).expect("a blame's origins fit its lines");
    for line in pending.iter() {
        origin[line.line as usize] = index;
    }
    let attributed = !pending.is_empty();
    pending.clear();
    attributed
}

/// The lines' origins as ranges: attributed lines by their origin, the `pending`
/// ones at their lines in view `oldest`.
fn ranges(origin: &[u32], pending: &[Pending], oldest: ViewId) -> Vec<BlameRange> {
    let mut ranges: Vec<BlameRange> = Vec::new();
    let mut pending = pending.iter().peekable();
    for (line, &index) in (0u32..).zip(origin) {
        let next = if index == UNRESOLVED {
            let entry = pending
                .next()
                .expect("every unresolved line is pending, in line order");
            debug_assert_eq!(entry.line, line);
            BlameOrigin::Unresolved {
                before: oldest,
                line: entry.at,
            }
        } else {
            BlameOrigin::Change {
                index: index as usize,
            }
        };
        if let Some(last) = ranges.last_mut() {
            let continues = match (last.origin, next) {
                (BlameOrigin::Change { index: a }, BlameOrigin::Change { index: b }) => a == b,
                (
                    BlameOrigin::Unresolved { line: first, .. },
                    BlameOrigin::Unresolved { line: at, .. },
                ) => first + last.len == at,
                _ => false,
            };
            if continues {
                last.len += 1;
                continue;
            }
        }
        ranges.push(BlameRange {
            start: line,
            len: 1,
            origin: next,
        });
    }
    ranges
}

/// The version of the file with content `id`, read when it is text of at most
/// `max_bytes`.
fn version<O>(
    reader: &RepositoryReader<'_, O>,
    id: ContentId,
    max_bytes: usize,
) -> Result<Version, BlameError> {
    let size = reader.content.logical_length(id)?;
    if size > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
        return Ok(Version::Unreadable { size });
    }
    let bytes = reader.content.get_bounded(id, max_bytes)?;
    Ok(match text_capture::classify(&bytes) {
        text_capture::CaptureKind::Text => Version::Text(bytes),
        text_capture::CaptureKind::Binary => Version::Binary,
    })
}

/// How many lines `bytes` holds, as the diff splits them.
fn line_count(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    newlines + usize::from(bytes.last().is_some_and(|&byte| byte != b'\n'))
}

/// `path`'s components: none for `""`, otherwise split at `/`.
fn components(path: &str) -> Result<Vec<String>, BlameError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    path.split('/')
        .map(|component| {
            if component.is_empty() {
                Err(BlameError::InvalidPath(path.to_owned()))
            } else {
                Ok(component.to_owned())
            }
        })
        .collect()
}
