//! Placing a workspace edit into an earlier change when later changes also edit the file.
//!
//! The workspace shows every change applied. An edit added to change B must therefore be
//! placed at B's spot: bytes only B had take the edit directly, while a block a later change
//! wrote moves into B wholesale when the edit overlaps it (see [`wheat_core::edit_placement`]).
//! The later changes' remaining edits are then replayed on the path. An edit that no longer
//! applies moves too, until the replay reproduces the workspace file exactly.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

use anyhow::{Result, bail};
use wheat_core::{
    ChangeId, ContentId, EditId, EditRecord, FileKind, FileValue, Tree, VerifiedContent,
    edit_application::{ApplicationLimits, ApplicationOutcome, ContentSource, apply_edits},
    edit_placement::{
        LaterEdit, Lines, LongFile, MoveNotice, PlacementInput, WholeFileReason, attribute, place,
        place_long,
    },
};
use wheat_store::{ContentStore, PathHistory};

use crate::outcome::CliFailure;
use crate::prompt::PromptIo;

/// A file's kind and canonical bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileBytes {
    pub kind: FileKind,
    pub bytes: Vec<u8>,
}

impl FileBytes {
    fn value(&self) -> FileValue {
        FileValue {
            content: ContentId::of(&self.bytes),
            kind: self.kind,
        }
    }
}

/// A version of a file to author.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Captured {
    /// Its canonical bytes.
    Bytes(FileBytes),
    /// A file past the guarded bound, already in the store.
    Stored(FileValue),
}

impl Captured {
    fn value(&self) -> FileValue {
        match self {
            Self::Bytes(file) => file.value(),
            Self::Stored(value) => *value,
        }
    }

    fn kind(&self) -> FileKind {
        match self {
            Self::Bytes(file) => file.kind,
            Self::Stored(value) => value.kind,
        }
    }
}

/// The earlier change's new version of one path and the later edits that move into it.
#[derive(Debug)]
pub struct PathPlacement {
    pub below: Option<Captured>,
    pub moved: BTreeSet<LaterEdit>,
    pub notices: Vec<MoveNotice>,
}

/// Places the workspace version of `path` at the earlier change's spot.
///
/// `current` is the path in the view the workspace shows, `workspace` its canonical workspace
/// version. Changes in `reauthored` get new revisions without their moved edits, so their
/// remaining edits must apply exactly; the others are reapplied and may find an edit already
/// satisfied.
///
/// A file one version of which is over `max_input_bytes` is never read: it moves as a whole
/// (see [`place_long`]).
#[tracing::instrument(level = "debug", skip_all, fields(path = path, later_steps = history.steps.len()))]
pub fn place_path(
    store: &ContentStore,
    path: &str,
    history: &PathHistory,
    current: Option<FileValue>,
    workspace: Option<&Captured>,
    reauthored: &BTreeSet<ChangeId>,
    max_input_bytes: usize,
) -> Result<PathPlacement> {
    let length = |value: Option<FileValue>| -> Result<Option<u64>> {
        value
            .map(|value| store.logical_length(value.content))
            .transpose()
            .map_err(Into::into)
    };
    let below_length = length(history.below)?;
    let long = matches!(workspace, Some(Captured::Stored(_)))
        || [below_length, length(current)?]
            .into_iter()
            .flatten()
            .any(|length| length > max_input_bytes as u64);
    let kind_changed = matches!((current, workspace), (Some(current), Some(workspace))
        if current.kind != workspace.kind());
    let wanted = workspace.map(Captured::value);
    if history.steps.is_empty() || (!kind_changed && current == wanted) {
        // Nothing later touched the path, or the author left it as it is.
        let below = if history.steps.is_empty() {
            workspace.cloned()
        } else {
            history
                .below
                .map(|value| captured(store, value, max_input_bytes))
                .transpose()?
        };
        return Ok(PathPlacement {
            below,
            moved: BTreeSet::new(),
            notices: Vec::new(),
        });
    }
    let corrupt =
        |_| CliFailure::corrupt(format!("later edit evidence for {path:?} is inconsistent"));
    let provenance = attribute(
        below_length
            .map(|length| {
                usize::try_from(length)
                    .map_err(|_| CliFailure::corrupt(format!("{path:?} exceeds memory")))
            })
            .transpose()?,
        &history.placement_steps(store)?,
    )
    .map_err(corrupt)?;
    let failed_before = history
        .steps
        .iter()
        .flat_map(|step| {
            step.edits
                .keys()
                .filter(|id| !step.applied.contains_key(id))
                .map(|edit| LaterEdit {
                    change: step.change,
                    edit: *edit,
                })
        })
        .collect::<BTreeSet<_>>();
    // Only a file every version of which fits the bound is read.
    let resolved = if long {
        None
    } else {
        let resolve = |value: Option<FileValue>| -> Result<Option<Vec<u8>>> {
            value
                .map(|value| store.get(value.content))
                .transpose()
                .map_err(Into::into)
        };
        let workspace = match workspace {
            Some(Captured::Bytes(file)) => Some(file.bytes.as_slice()),
            Some(Captured::Stored(_)) => unreachable!("a stored workspace file is long"),
            None => None,
        };
        Some((resolve(history.below)?, resolve(current)?, workspace))
    };
    let mut also_move = BTreeSet::new();
    loop {
        let (placed, moved, notices) = match &resolved {
            Some((below, current, workspace_bytes)) => {
                let placement = place(PlacementInput {
                    below: below.as_deref(),
                    current: current.as_deref(),
                    workspace: *workspace_bytes,
                    kind_changed,
                    provenance: &provenance,
                    also_move: &also_move,
                })
                .map_err(corrupt)?;
                let placed = placement.below.map(|bytes| {
                    Captured::Bytes(FileBytes {
                        kind: workspace.map_or(FileKind::Regular, Captured::kind),
                        bytes,
                    })
                });
                (placed, placement.moved, placement.notices)
            }
            None => {
                let moves = place_long(
                    LongFile {
                        below: history.below.is_some(),
                        current: current.is_some(),
                        workspace: workspace.is_some(),
                        kind_changed,
                    },
                    &provenance,
                    &also_move,
                );
                (workspace.cloned(), moves.moved, moves.notices)
            }
        };
        let replay = replay(
            store,
            path,
            history,
            placed.as_ref(),
            &moved,
            &failed_before,
            reauthored,
        )?;
        match replay {
            Replay::Reached(value) if value == wanted => {
                return Ok(PathPlacement {
                    below: placed,
                    moved,
                    notices,
                });
            }
            Replay::Reached(_) => {
                return Err(CliFailure::conflict(format!(
                    "cannot place the workspace version of {path:?}: later changes' edits no longer reproduce it"
                )));
            }
            Replay::NoLongerFit(edits) => also_move.extend(edits),
        }
    }
}

/// `value` as a version to author: its bytes when it fits `max_input_bytes`, else the value.
fn captured(store: &ContentStore, value: FileValue, max_input_bytes: usize) -> Result<Captured> {
    Ok(
        if store.logical_length(value.content)? > max_input_bytes as u64 {
            Captured::Stored(value)
        } else {
            Captured::Bytes(FileBytes {
                kind: value.kind,
                bytes: store.get(value.content)?,
            })
        },
    )
}

enum Replay {
    /// Every remaining later edit applied, ending at this value.
    Reached(Option<FileValue>),
    /// These later edits no longer apply.
    NoLongerFit(BTreeSet<LaterEdit>),
}

/// Applies the later changes' remaining edits on `path`, starting from `placed`.
#[tracing::instrument(level = "debug", skip_all, fields(moved = moved.len()))]
fn replay(
    store: &ContentStore,
    path: &str,
    history: &PathHistory,
    placed: Option<&Captured>,
    moved: &BTreeSet<LaterEdit>,
    failed_before: &BTreeSet<LaterEdit>,
    reauthored: &BTreeSet<ChangeId>,
) -> Result<Replay> {
    // Bytes travel verified, so a step hashes only the content it generates.
    let mut file: Option<(FileKind, Held<'_>)> = placed.map(|file| match file {
        Captured::Bytes(file) => (
            file.kind,
            Held::Bytes(VerifiedContent::new(Cow::Borrowed(file.bytes.as_slice()))),
        ),
        Captured::Stored(value) => (value.kind, Held::Stored(value.content)),
    });
    let value_of = |(kind, held): &(FileKind, Held<'_>)| FileValue {
        content: held.id(),
        kind: *kind,
    };
    let mut no_longer_fit = BTreeSet::new();
    for step in &history.steps {
        let later = |edit: EditId| LaterEdit {
            change: step.change,
            edit,
        };
        let edits = step
            .edits
            .iter()
            .filter(|(id, _)| !moved.contains(&later(**id)))
            .map(|(id, edit)| (*id, edit.clone()))
            .collect::<BTreeMap<EditId, EditRecord>>();
        if edits.is_empty() {
            continue;
        }
        let reapplied = !reauthored.contains(&step.change);
        let mut target = Tree::new();
        let value = file.as_ref().map(value_of);
        if let Some(value) = value {
            target.insert(path.to_owned(), value);
        }
        let mut result = apply_edits(
            &target,
            edits,
            Replaying {
                store,
                file: file.as_ref().and_then(|(_, held)| match held {
                    Held::Bytes(content) => Some(content),
                    Held::Stored(_) => None,
                }),
            },
            ApplicationLimits {
                allow_already_satisfied: reapplied,
                ..ApplicationLimits::default()
            },
        )
        .map_err(|error| anyhow::anyhow!("cannot replay later edits on {path:?}: {error:?}"))?;
        for (id, outcome) in &result.outcomes {
            let fits = match outcome {
                ApplicationOutcome::Applied { .. } => true,
                ApplicationOutcome::AlreadySatisfied { .. } => reapplied,
                _ => false,
            };
            if fits {
                continue;
            }
            if !failed_before.contains(&later(*id)) {
                no_longer_fit.insert(later(*id));
            } else if !reapplied {
                bail!(CliFailure::conflict(format!(
                    "change {} is conflicted on {path:?}; resolve it before moving its edits",
                    step.change
                )));
            }
        }
        if !no_longer_fit.is_empty() {
            return Ok(Replay::NoLongerFit(no_longer_fit));
        }
        file = match result.tree.get(path).copied() {
            None => None,
            Some(next) if Some(next) == value => file,
            // Content an edit names is read only when a later step reads it.
            Some(next) => Some((
                next.kind,
                match result.generated_content.remove(&next.content) {
                    Some(content) => Held::Bytes(content.into()),
                    None => Held::Stored(next.content),
                },
            )),
        };
    }
    Ok(Replay::Reached(file.as_ref().map(value_of)))
}

/// The path's content as a replay holds it.
enum Held<'a> {
    Bytes(VerifiedContent<Cow<'a, [u8]>>),
    /// Content only the store has, read when an edit needs it.
    Stored(ContentId),
}

impl Held<'_> {
    fn id(&self) -> ContentId {
        match self {
            Self::Bytes(content) => content.id(),
            Self::Stored(id) => *id,
        }
    }
}

/// The content a replay step reads: the file as the replay holds it, else the store.
struct Replaying<'a> {
    store: &'a ContentStore,
    file: Option<&'a VerifiedContent<Cow<'a, [u8]>>>,
}

impl ContentSource for Replaying<'_> {
    type Error = wheat_store::ContentStoreError;

    fn resolve(&mut self, id: ContentId) -> Result<VerifiedContent, Self::Error> {
        match self.file {
            Some(content) if id == content.id() => Ok(content.borrowed().into_owned()),
            _ => self.store.get_verified(id),
        }
    }

    fn confirm(&mut self, id: ContentId) -> Result<Option<VerifiedContent>, Self::Error> {
        match self.file {
            Some(content) if id == content.id() => Ok(None),
            _ => self.store.confirm(id).map(|()| None),
        }
    }

    fn logical_length(&mut self, id: ContentId) -> Result<Option<u64>, Self::Error> {
        match self.file {
            Some(content) if id == content.id() => Ok(Some(content.bytes().len() as u64)),
            _ => self.store.logical_length(id).map(Some),
        }
    }
}

/// Asks before moving edits out of later changes into `change`. Without anyone to ask, it
/// lists the moves and refuses; `--force` skips the question.
pub fn confirm(change: ChangeId, moves: &[String], io: &mut PromptIo<'_>) -> Result<()> {
    let mut listing = format!("adding to change {change} moves edits later changes made:\n");
    for line in moves {
        listing.push_str("  ");
        listing.push_str(line);
        listing.push('\n');
    }
    if !io.interactive {
        bail!(CliFailure::would_move(format!(
            "{listing}rerun with --force to move them"
        )));
    }
    write!(io.diagnostics, "{listing}")?;
    if io.ask_yes_no("Proceed?")? {
        Ok(())
    } else {
        bail!(CliFailure::would_move("add cancelled; nothing was changed"))
    }
}

fn lines(lines: Lines) -> String {
    if lines.first == lines.last {
        format!("line {}", lines.first)
    } else {
        format!("lines {}-{}", lines.first, lines.last)
    }
}

fn changes(changes: &BTreeSet<ChangeId>) -> String {
    let names = changes.iter().map(ToString::to_string).collect::<Vec<_>>();
    match names.as_slice() {
        [one] => format!("change {one}"),
        many => format!("changes {}", many.join(", ")),
    }
}

/// One line explaining a move into `into`, for the confirmation prompt and the refusal.
pub fn describe(path: &str, into: ChangeId, notice: &MoveNotice) -> String {
    match notice {
        MoveNotice::WholeFile {
            reason,
            changes: from,
        } => {
            let from = changes(from);
            let why = match reason {
                WholeFileReason::CreatedLater => {
                    format!("the file doesn't exist in change {into}; {from} created it")
                }
                WholeFileReason::DeletedLater => format!("{from} deleted the file later"),
                WholeFileReason::ReplacedLater => format!("{from} replaced the whole file later"),
                WholeFileReason::WorkspaceReplaces => {
                    format!("your edit replaces the whole file, which {from} also edits")
                }
                WholeFileReason::Long => {
                    format!(
                        "the file is too long to place block by block, and {from} also edits it"
                    )
                }
            };
            format!("{path}: {why}; adding it to {into} moves the entire file into {into}")
        }
        MoveNotice::Block {
            edit,
            yours,
            theirs,
        } => format!(
            "{path}: your edit on {} overlaps change {}'s edit on {}; that block moves into {into} wholesale",
            lines(*yours),
            edit.change,
            lines(*theirs),
        ),
        MoveNotice::NoLongerFits { edit, theirs } => format!(
            "{path}: change {}'s edit on {} no longer fits after your edit; it moves into {into} too",
            edit.change,
            lines(*theirs),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(answer: &str, interactive: bool) -> (Result<()>, String) {
        let mut input = answer.as_bytes();
        let mut diagnostics = Vec::new();
        let result = confirm(
            ChangeId::new(),
            &["file: moves".to_owned()],
            &mut PromptIo {
                input: &mut input,
                diagnostics: &mut diagnostics,
                interactive,
            },
        );
        (result, String::from_utf8(diagnostics).unwrap())
    }

    fn would_move(result: &Result<()>) -> bool {
        result.as_ref().is_err_and(|error| {
            matches!(
                error.downcast_ref::<CliFailure>(),
                Some(CliFailure::EditsWouldMove(_))
            )
        })
    }

    #[test]
    fn a_yes_proceeds() {
        for answer in ["y\n", "YES\n", " yes \n"] {
            let (result, prompt) = ask(answer, true);
            assert!(result.is_ok(), "{answer:?}");
            assert!(
                prompt.contains("  file: moves\nProceed? [y/N] "),
                "{prompt}"
            );
        }
    }

    #[test]
    fn anything_else_cancels() {
        for answer in ["\n", "n\n", "yep\n", ""] {
            let (result, _) = ask(answer, true);
            assert!(would_move(&result), "{answer:?}");
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("nothing was changed")
            );
        }
    }

    #[test]
    fn without_a_terminal_it_lists_the_moves_and_asks_for_force() {
        let (result, prompt) = ask("y\n", false);
        assert!(prompt.is_empty(), "nothing is asked: {prompt}");
        assert!(would_move(&result));
        let message = result.unwrap_err().to_string();
        assert!(message.contains("  file: moves\n"), "{message}");
        assert!(
            message.ends_with("rerun with --force to move them"),
            "{message}"
        );
    }
}
