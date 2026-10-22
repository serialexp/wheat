//! Where the screens' data comes from and where their actions go. A backend runs
//! on its own thread so a slow store never holds up a frame; the window drains
//! its updates between frames. Tests and screenshots answer inline instead.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::fixture;
use crate::model::{Change, Diff, FileGroup, Model, Revision};

/// Wakes the window's event loop when an update is ready.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Where a capture's paths go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// The workspace's selected change.
    Selected,
    /// Another change, by id.
    Change(String),
    /// A new change, built on the frontier, on chosen changes, or on nothing.
    New(NewBase),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NewBase {
    Frontier,
    Changes(Vec<String>),
    Independent,
}

#[derive(Clone, Debug)]
pub enum Request {
    Load,
    WorkspaceDiff {
        path: String,
    },
    RevisionDiff {
        change: String,
        revision: usize,
    },
    Add {
        paths: Vec<String>,
        target: Target,
        message: String,
    },
    Restore {
        paths: Vec<String>,
    },
    SelectChange {
        change: String,
    },
    SquashChange {
        change: String,
    },
    SquashStack {
        change: String,
    },
    Refresh,
    SwitchFrontier {
        frontier: String,
    },
}

#[derive(Clone, Debug)]
pub enum Update {
    Loaded(Result<Model, String>),
    WorkspaceDiff {
        path: String,
        diff: Diff,
    },
    RevisionDiff {
        change: String,
        revision: usize,
        diff: Diff,
    },
    /// An action finished. `Ok` carries a toast's title and message; a reload
    /// follows either way.
    Done {
        action: String,
        outcome: Result<(String, String), String>,
    },
}

/// A data source: the comp's fixture, or a live workspace.
pub trait Backend: Send {
    fn handle(&mut self, request: Request) -> Vec<Update>;
}

/// The window's end of a backend.
pub struct Client {
    requests: Option<Sender<Request>>,
    updates: Receiver<Update>,
    /// Inline backends answer as they are asked; tests and screenshots use one.
    inline: Option<(Box<dyn Backend>, Sender<Update>)>,
}

impl Client {
    /// A backend on its own thread, waking the window with each update.
    pub fn threaded(mut backend: Box<dyn Backend>, wake: Wake) -> Self {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, updates) = mpsc::channel();
        std::thread::Builder::new()
            .name("wheat-backend".into())
            .spawn(move || {
                for request in inbox {
                    for update in backend.handle(request) {
                        if outbox.send(update).is_err() {
                            return;
                        }
                        wake();
                    }
                }
            })
            .expect("spawn the backend thread");
        Self {
            requests: Some(requests),
            updates,
            inline: None,
        }
    }

    /// A backend answering on the caller's thread.
    pub fn inline(backend: Box<dyn Backend>) -> Self {
        let (outbox, updates) = mpsc::channel();
        Self {
            requests: None,
            updates,
            inline: Some((backend, outbox)),
        }
    }

    pub fn send(&mut self, request: Request) {
        if let Some((backend, outbox)) = self.inline.as_mut() {
            for update in backend.handle(request) {
                let _ = outbox.send(update);
            }
        } else if let Some(requests) = &self.requests {
            let _ = requests.send(request);
        }
    }

    pub fn try_recv(&self) -> Option<Update> {
        self.updates.try_recv().ok()
    }
}

/// A source this build can't open yet: every load fails with why, so the
/// window says so instead of showing data that isn't the repository's.
pub struct Unavailable(pub String);

impl Backend for Unavailable {
    fn handle(&mut self, request: Request) -> Vec<Update> {
        match request {
            Request::Load | Request::Refresh => vec![Update::Loaded(Err(self.0.clone()))],
            other => vec![Update::Done {
                action: action_name(&other),
                outcome: Err(self.0.clone()),
            }],
        }
    }
}

/// The comp's demo repository. Its actions change the demo the way the comp's
/// prototype does, so every control can be tried without a store.
pub struct FixtureBackend {
    model: Model,
}

impl FixtureBackend {
    pub fn new() -> Self {
        Self {
            model: fixture::model(),
        }
    }

    fn index(&self, change: &str) -> Option<usize> {
        self.model
            .changes
            .iter()
            .position(|candidate| candidate.id == change)
    }

    /// A new change, newest so first in the list, without revisions yet. Every
    /// index into `changes` moves down one.
    fn create(&mut self, title: &str, base: NewBase) -> Result<usize, String> {
        let depends_on = match &base {
            NewBase::Frontier | NewBase::Independent => Vec::new(),
            NewBase::Changes(ids) => ids
                .iter()
                .map(|id| {
                    self.index(id)
                        .map(|index| index + 1)
                        .ok_or("no such change")
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        let model = &mut self.model;
        for change in &mut model.changes {
            for base in &mut change.depends_on {
                *base += 1;
            }
        }
        for selected in model
            .workspaces
            .iter_mut()
            .map(|workspace| &mut workspace.selected)
            .chain([&mut model.workspace.selected])
        {
            *selected = selected.map(|index| index + 1);
        }
        let short = format!("n{:03x}", model.store.changes % 0xfff);
        model.changes.insert(
            0,
            Change {
                id: format!("{short}0000-0000-0000-0000-000000000000"),
                short,
                title: title.to_owned(),
                author: "you".into(),
                age: "now".into(),
                depends_on,
                independent: base == NewBase::Independent,
                ..Change::default()
            },
        );
        model.store.changes += 1;
        model.store.unpublished_changes += 1;
        model.index_dependents();
        Ok(0)
    }

    fn act(&mut self, request: Request) -> Result<(String, String), String> {
        match request {
            Request::Add {
                paths,
                target,
                message,
            } => {
                let title = message.lines().next().unwrap_or_default().trim().to_owned();
                let index = match target {
                    Target::Selected => {
                        self.model.workspace.selected.ok_or("no selected change")?
                    }
                    Target::Change(id) => self.index(&id).ok_or("no such change")?,
                    Target::New(base) => {
                        if title.is_empty() {
                            return Err("a new change needs a message".into());
                        }
                        self.create(&title, base)?
                    }
                };
                self.model
                    .workspace
                    .files
                    .retain(|file| !paths.contains(&file.path));
                let change = &mut self.model.changes[index];
                if change.revisions.is_empty() {
                    change.revisions.push(Revision {
                        message: title,
                        actor: "you".into(),
                        age: "now".into(),
                    });
                    return Ok((
                        format!("Created {}", change.short),
                        format!("{} paths captured in “{}”.", paths.len(), change.title),
                    ));
                }
                change.revisions.push(Revision {
                    message: if title.is_empty() {
                        "Capture local edits".into()
                    } else {
                        title
                    },
                    actor: "you".into(),
                    age: "now".into(),
                });
                change.age = "now".into();
                Ok((
                    format!("{} r{}", change.short, change.revisions.len()),
                    format!("{} paths added to “{}”.", paths.len(), change.title),
                ))
            }
            Request::Restore { paths } => {
                self.model.workspace.files.retain(|file| {
                    !(paths.contains(&file.path) && file.group == FileGroup::Tracked)
                });
                Ok((
                    "Restored".into(),
                    format!("{} paths back to the materialized tree.", paths.len()),
                ))
            }
            Request::SelectChange { change } => {
                let index = self.index(&change).ok_or("no such change")?;
                self.model.workspace.selected = Some(index);
                Ok((
                    format!("Selected {}", self.model.changes[index].short),
                    "`add` now goes to this change.".into(),
                ))
            }
            Request::SquashChange { change } => {
                let index = self.index(&change).ok_or("no such change")?;
                let change = &mut self.model.changes[index];
                let count = change.revisions.len();
                change.revisions.truncate(1);
                Ok((
                    format!("Squashed {}", change.short),
                    format!("{count} unpublished revisions collapsed into one."),
                ))
            }
            Request::SquashStack { change } => {
                let index = self.index(&change).ok_or("no such change")?;
                let mut stack = vec![index];
                stack.extend(self.model.dependents(index));
                for &at in &stack {
                    if !self.model.changes[at].published {
                        self.model.changes[at].revisions.truncate(1);
                    }
                }
                Ok((
                    "Squashed stack".into(),
                    format!("{} changes now have one revision each.", stack.len()),
                ))
            }
            Request::Refresh => {
                self.model.workspace.moved = None;
                Ok(("Refreshed".into(), "Rematerialized the frontier.".into()))
            }
            Request::SwitchFrontier { frontier } => {
                self.model.workspace.frontier = Some(frontier.clone());
                Ok((
                    format!("Materialized {}/{frontier}", self.model.workspace.label),
                    String::new(),
                ))
            }
            Request::Load | Request::WorkspaceDiff { .. } | Request::RevisionDiff { .. } => {
                unreachable!("not an action")
            }
        }
    }
}

impl Backend for FixtureBackend {
    fn handle(&mut self, request: Request) -> Vec<Update> {
        match request {
            Request::Load => vec![Update::Loaded(Ok(self.model.clone()))],
            Request::WorkspaceDiff { path } => {
                let diff = fixture::workspace_diff(&self.model, &path);
                vec![Update::WorkspaceDiff { path, diff }]
            }
            Request::RevisionDiff { change, revision } => {
                let diff = self.index(&change).map_or(Diff::None, |index| {
                    fixture::revision_diff(&self.model, index, revision)
                });
                vec![Update::RevisionDiff {
                    change,
                    revision,
                    diff,
                }]
            }
            action => {
                let name = action_name(&action);
                let outcome = self.act(action);
                vec![
                    Update::Done {
                        action: name,
                        outcome,
                    },
                    Update::Loaded(Ok(self.model.clone())),
                ]
            }
        }
    }
}

/// The status bar's "last:" line for an action.
pub fn action_name(request: &Request) -> String {
    match request {
        Request::Load => "load".into(),
        Request::WorkspaceDiff { path } => format!("diff {path}"),
        Request::RevisionDiff { revision, .. } => format!("diff r{revision}"),
        Request::Add { paths, .. } => format!("add {} paths", paths.len()),
        Request::Restore { paths } => format!("restore {} paths", paths.len()),
        Request::SelectChange { .. } => "select change".into(),
        Request::SquashChange { .. } => "squash change".into(),
        Request::SquashStack { .. } => "squash stack".into(),
        Request::Refresh => "refresh".into(),
        Request::SwitchFrontier { frontier } => format!("materialize {frontier}"),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn act(backend: &mut FixtureBackend, request: Request) -> Result<(String, String), String> {
        match backend.handle(request).into_iter().next() {
            Some(Update::Done { outcome, .. }) => outcome,
            other => panic!("expected an action's outcome, got {other:?}"),
        }
    }

    #[test]
    fn a_new_change_goes_first_and_every_index_follows_it() {
        let mut backend = FixtureBackend::new();
        let before = backend.model.clone();
        let selected = before
            .workspace
            .selected
            .expect("the fixture selects a change");
        let base = before.changes[2].id.clone();
        act(
            &mut backend,
            Request::Add {
                paths: vec!["src/lexer/mod.rs".into()],
                target: Target::New(NewBase::Changes(vec![base.clone()])),
                message: "Split the lexer module\n\nbody".into(),
            },
        )
        .expect("the change is created");
        let model = &backend.model;
        assert_eq!(model.changes.len(), before.changes.len() + 1);
        let created = &model.changes[0];
        assert_eq!(created.title, "Split the lexer module");
        assert_eq!(created.revisions.len(), 1);
        assert_eq!(model.changes[created.depends_on[0]].id, base);
        assert_eq!(model.workspace.selected, Some(selected + 1));
        assert_eq!(model.changes[selected + 1].id, before.changes[selected].id);
        for (index, change) in before.changes.iter().enumerate() {
            let moved: Vec<&str> = model.changes[index + 1]
                .depends_on
                .iter()
                .map(|&at| model.changes[at].id.as_str())
                .collect();
            let was: Vec<&str> = change
                .depends_on
                .iter()
                .map(|&at| before.changes[at].id.as_str())
                .collect();
            assert_eq!(moved, was, "{} keeps its bases", change.short);
        }
        assert!(
            model.changes[3].dependents.contains(&0),
            "the base lists the new change"
        );
        assert!(
            !model
                .workspace
                .files
                .iter()
                .any(|file| file.path == "src/lexer/mod.rs")
        );
    }

    #[test]
    fn a_failed_capture_keeps_the_files() {
        let mut backend = FixtureBackend::new();
        let files = backend.model.workspace.files.len();
        let outcome = act(
            &mut backend,
            Request::Add {
                paths: vec!["src/lexer/mod.rs".into()],
                target: Target::Change("nonexistent".into()),
                message: String::new(),
            },
        );
        assert!(outcome.is_err());
        assert_eq!(backend.model.workspace.files.len(), files);
    }

    #[test]
    fn a_new_change_needs_a_message() {
        let mut backend = FixtureBackend::new();
        let outcome = act(
            &mut backend,
            Request::Add {
                paths: vec!["src/lexer/mod.rs".into()],
                target: Target::New(NewBase::Frontier),
                message: "  \n".into(),
            },
        );
        assert!(outcome.is_err());
    }

    #[test]
    fn an_unavailable_source_fails_every_load() {
        let mut backend = Unavailable("no".into());
        assert!(matches!(
            backend.handle(Request::Load).as_slice(),
            [Update::Loaded(Err(error))] if error == "no"
        ));
    }
}
