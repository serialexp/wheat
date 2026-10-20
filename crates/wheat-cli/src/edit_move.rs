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
    ChangeId, ContentId, ContentIo, EditId, EditRecord, FileKind, FileValue, Tree,
    edit_application::{ApplicationLimits, ApplicationOutcome, apply_edits},
    edit_placement::{
        LaterEdit, Lines, MoveNotice, PlacementInput, WholeFileReason, attribute, place,
    },
};
use wheat_store::{ContentStore, PathHistory};

use crate::command_output::CliFailure;
use crate::prompt::PromptIo;

/// A file's kind and canonical bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FileBytes {
    pub(crate) kind: FileKind,
    pub(crate) bytes: Vec<u8>,
}

impl FileBytes {
    fn value(&self) -> FileValue {
        FileValue {
            content: ContentId::of(&self.bytes),
            kind: self.kind,
        }
    }
}

/// The earlier change's new version of one path and the later edits that move into it.
#[derive(Debug)]
pub(crate) struct PathPlacement {
    pub(crate) below: Option<FileBytes>,
    pub(crate) moved: BTreeSet<LaterEdit>,
    pub(crate) notices: Vec<MoveNotice>,
}

/// Places the workspace version of `path` at the earlier change's spot.
///
/// `current` is the path in the view the workspace shows, `workspace` its canonical workspace
/// version. Changes in `reauthored` get new revisions without their moved edits, so their
/// remaining edits must apply exactly; the others are reapplied and may find an edit already
/// satisfied.
#[tracing::instrument(level = "debug", skip_all, fields(path = path, later_steps = history.steps.len()))]
pub(crate) fn place_path(
    store: &ContentStore,
    path: &str,
    history: &PathHistory,
    current: Option<FileValue>,
    workspace: Option<&FileBytes>,
    reauthored: &BTreeSet<ChangeId>,
) -> Result<PathPlacement> {
    let resolve = |value: Option<FileValue>| -> Result<Option<Vec<u8>>> {
        value
            .map(|value| store.resolve(value.content))
            .transpose()
            .map_err(Into::into)
    };
    let below = resolve(history.below)?;
    let current_bytes = resolve(current)?;
    let kind_changed = matches!((current, workspace), (Some(current), Some(workspace))
        if current.kind != workspace.kind);
    let unchanged =
        !kind_changed && current_bytes.as_deref() == workspace.map(|file| file.bytes.as_slice());
    if history.steps.is_empty() || unchanged {
        // Nothing later touched the path, or the author left it as it is.
        let below = if history.steps.is_empty() {
            workspace.cloned()
        } else {
            history.below.zip(below).map(|(value, bytes)| FileBytes {
                kind: value.kind,
                bytes,
            })
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
        below.as_ref().map(Vec::len),
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
    let wanted = workspace.map(FileBytes::value);
    let mut also_move = BTreeSet::new();
    loop {
        let placement = place(PlacementInput {
            below: below.as_deref(),
            current: current_bytes.as_deref(),
            workspace: workspace.map(|file| file.bytes.as_slice()),
            kind_changed,
            provenance: &provenance,
            also_move: &also_move,
        })
        .map_err(corrupt)?;
        let placed = placement.below.map(|bytes| FileBytes {
            kind: workspace.map_or(FileKind::Regular, |file| file.kind),
            bytes,
        });
        let replay = replay(
            store,
            path,
            history,
            placed.as_ref(),
            &placement.moved,
            &failed_before,
            reauthored,
        )?;
        match replay {
            Replay::Reached(value) if value == wanted => {
                return Ok(PathPlacement {
                    below: placed,
                    moved: placement.moved,
                    notices: placement.notices,
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
    placed: Option<&FileBytes>,
    moved: &BTreeSet<LaterEdit>,
    failed_before: &BTreeSet<LaterEdit>,
    reauthored: &BTreeSet<ChangeId>,
) -> Result<Replay> {
    // The value travels with the bytes, so a step hashes only the content it generates.
    let mut file: Option<(FileValue, Cow<'_, [u8]>)> =
        placed.map(|file| (file.value(), Cow::Borrowed(file.bytes.as_slice())));
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
        let value = file.as_ref().map(|(value, _)| *value);
        if let Some(value) = value {
            target.insert(path.to_owned(), value);
        }
        let mut result = apply_edits(
            &target,
            edits,
            |id| match &file {
                Some((value, bytes)) if id == value.content => Ok(bytes.to_vec()),
                _ => store.resolve(id).map_err(|error| error.to_string()),
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
            Some(next) => {
                let bytes = match result.generated_content.remove(&next.content) {
                    Some(content) => content.into_inner(),
                    None => store.resolve(next.content)?,
                };
                Some((next, Cow::Owned(bytes)))
            }
        };
    }
    Ok(Replay::Reached(file.map(|(value, _)| value)))
}

/// Asks before moving edits out of later changes into `change`. Without anyone to ask, it
/// lists the moves and refuses; `--force` skips the question.
pub(crate) fn confirm(change: ChangeId, moves: &[String], io: &mut PromptIo<'_>) -> Result<()> {
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
pub(crate) fn describe(path: &str, into: ChangeId, notice: &MoveNotice) -> String {
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
