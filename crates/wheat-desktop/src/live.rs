//! A workspace on disk, read and changed through wheat-client. Each request opens
//! the store, takes its locks for that one operation and lets them go, so the
//! `wheat` command can work in the same workspace while the window is open.

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use wheat_client::authoring::{self, AddPaths, AddRequest, NewChange};
use wheat_client::authoring_metadata::MetadataArgs;
use wheat_client::dot_git::DotGitGate;
use wheat_client::squash_attribution::Coauthors;
use wheat_client::{
    active_changes, frontier_service, history, squash, workspace_diff, workspace_lifecycle,
    workspace_status,
};
use wheat_core::diff_model::{
    DiffDocument, DiffLimits, DiffLineOperation, DiffPresentation, DiffText, FileChangeKind,
    FileDiff,
};
use wheat_core::{ChangeId, RevisionEdits, RevisionId, Timestamp, text_capture};
use wheat_store::TreePathFilter;

use crate::backend::{Backend, NewBase, Request, Target, Update, action_name};
use crate::model::{
    Change, Diff, DiffLine, FileCode, FileEntry, FileGroup, FrontierChoice, LineKind, Model,
    Revision, StoreSummary, WorkspaceEntry, WorkspaceState, WorkspaceView,
};

/// The most revisions one expand reads; a longer chain shows its newest.
const MAX_REVISIONS: usize = 4096;
/// The most of an untracked file read to show it.
const MAX_UNTRACKED_BYTES: u64 = 1024 * 1024;

pub struct LiveBackend {
    workspace: PathBuf,
    /// Each change's current revision, by change id, as the last load found it.
    current: HashMap<String, RevisionId>,
    /// Paths the last load found untracked or ignored, which have no tracked diff.
    untracked: HashSet<String>,
}

impl LiveBackend {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            current: HashMap::new(),
            untracked: HashSet::new(),
        }
    }

    fn load(&mut self) -> Result<Model> {
        let root = self.workspace.as_path();
        let status = workspace_status::read(root, None)?;
        let active = active_changes::read(root, None)?;
        let entries = history::read(root, None)?;
        let workspaces = workspace_lifecycle::entries(root, None)?;
        let frontiers = frontier_service::read(root, None)?;
        let now = now();

        let index: HashMap<ChangeId, usize> = entries
            .iter()
            .enumerate()
            .map(|(at, entry)| (entry.change, at))
            .collect();
        let local: HashMap<ChangeId, bool> = active
            .changes
            .iter()
            .map(|change| (change.change, change.conflicted))
            .collect();
        let mut store = StoreSummary {
            changes: entries.len(),
            ..StoreSummary::default()
        };
        self.current.clear();
        let mut changes = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = entry.change.to_string();
            self.current.insert(id.clone(), entry.revision);
            let revision_count = usize::try_from(entry.generation)
                .unwrap_or(usize::MAX)
                .saturating_add(1);
            // The label's changes are the published ones; the working view's own
            // changes above it are this workspace's.
            let conflicted = local.get(&entry.change).copied();
            if let Some(conflicted) = conflicted {
                store.unpublished_changes += 1;
                store.unpublished_revisions += revision_count;
                store.conflicted_changes += usize::from(conflicted);
            }
            let age = age(entry.committed_at, now);
            changes.push(Change {
                short: short_change(&id),
                id,
                title: entry.title,
                author: entry.creator.name,
                agent: false,
                age: age.clone(),
                revision_count,
                revisions: vec![Revision {
                    id: entry.revision.to_string(),
                    message: entry.message,
                    actor: entry.author.name,
                    age,
                }],
                depends_on: entry
                    .dependencies
                    .iter()
                    .filter_map(|dependency| index.get(dependency).copied())
                    .collect(),
                dependents: Vec::new(),
                independent: entry.dependencies.is_empty(),
                published: conflicted.is_none(),
                conflicted: conflicted.unwrap_or(false),
            });
        }

        let selected = active
            .selected
            .and_then(|change| index.get(&change).copied());
        self.untracked.clear();
        let mut files = Vec::with_capacity(status.changes.len() + status.untracked.len());
        for change in &status.changes {
            files.push(FileEntry {
                path: change.path.clone(),
                code: if change.deleted {
                    FileCode::Deleted
                } else if change.kind_changed {
                    FileCode::TypeChanged
                } else {
                    FileCode::Modified
                },
                group: FileGroup::Tracked,
                stat: String::new(),
            });
        }
        for file in &status.untracked {
            let mut path = file.path.clone();
            if file.is_directory && !path.ends_with('/') {
                path.push('/');
            }
            self.untracked.insert(path.clone());
            files.push(FileEntry {
                path,
                code: if file.ignored {
                    FileCode::Ignored
                } else {
                    FileCode::Untracked
                },
                group: if file.ignored {
                    FileGroup::Ignored
                } else {
                    FileGroup::Untracked
                },
                stat: String::new(),
            });
        }

        // With several frontiers below the label and none selected, the workspace
        // waits on the basis for a choice.
        let frontier = match status.selected_frontier {
            Some(view) => Some(short(&view.to_string())),
            None if frontiers.frontiers.len() > 1 => None,
            None => Some(short(&status.effective_view.to_string())),
        };
        let mut model = Model {
            name: display_name(root),
            path: display_path(root),
            workspaces: workspaces
                .into_iter()
                .map(|entry| WorkspaceEntry {
                    name: display_name(&entry.path),
                    frontier: short(&entry.effective_view.to_string()),
                    selected: entry.current.then_some(selected).flatten(),
                    current: entry.current,
                    agent: None,
                    state: WorkspaceState::Idle,
                    task: None,
                })
                .collect(),
            store,
            workspace: WorkspaceView {
                label: status.active_label.clone(),
                frontier,
                frontiers: frontiers
                    .frontiers
                    .iter()
                    .map(|view| FrontierChoice {
                        name: short(&view.to_string()),
                        changes: None,
                    })
                    .collect(),
                selected,
                moved: status.authority_advanced.then(|| {
                    "The repository moved since this workspace was materialized.".to_owned()
                }),
                files,
            },
            changes,
            older_changes: 0,
        };
        model.index_dependents();
        Ok(model)
    }

    fn workspace_diff(&self, path: &str) -> Diff {
        let result = if self.untracked.contains(path) {
            untracked_diff(&self.workspace, path)
        } else {
            workspace_diff::compare(
                &self.workspace,
                None,
                workspace_diff::Comparison::Workspace,
                &TreePathFilter::new(vec![path.to_owned()]),
                DiffLimits::default(),
            )
            .map(|document| document_diff(&document))
        };
        result.unwrap_or_else(|error| Diff::Failed(format!("{error:#}")))
    }

    fn revision_diff(&self, id: &str) -> Diff {
        RevisionId::from_str(id)
            .map_err(|_| anyhow::anyhow!("invalid revision ID {id}"))
            .and_then(|revision| {
                workspace_diff::compare(
                    &self.workspace,
                    None,
                    workspace_diff::Comparison::Revision(revision),
                    &TreePathFilter::default(),
                    DiffLimits::default(),
                )
            })
            .map(|document| document_diff(&document))
            .unwrap_or_else(|error| Diff::Failed(format!("{error:#}")))
    }

    fn revisions(&self, change: &str) -> Result<(Vec<Revision>, bool)> {
        let current = *self
            .current
            .get(change)
            .with_context(|| format!("change {change} is not loaded"))?;
        let mut chain = history::revisions(&self.workspace, None, current, MAX_REVISIONS + 1)?;
        let complete = chain.len() <= MAX_REVISIONS;
        chain.truncate(MAX_REVISIONS);
        let now = now();
        Ok((
            chain
                .into_iter()
                .rev()
                .map(|entry| Revision {
                    id: entry.revision.to_string(),
                    message: entry.message,
                    actor: entry.author.name,
                    age: age(entry.committed_at, now),
                })
                .collect(),
            complete,
        ))
    }

    fn act(&mut self, request: Request) -> Result<(String, String)> {
        let root = self.workspace.clone();
        let root = root.as_path();
        match request {
            Request::Add {
                paths,
                target,
                message,
            } => {
                let change = match target {
                    Target::Selected => None,
                    Target::Change(id) => Some(parse_change(&id)?),
                    Target::New(base) => Some(create(root, &message, base)?),
                };
                let count = paths.len();
                let added = authoring::add(
                    root,
                    None,
                    AddRequest {
                        paths: AddPaths::Listed(paths),
                        change,
                        predecessor: None,
                        guards: Default::default(),
                        message,
                        authored_at: None,
                        committed_at: None,
                    },
                    |_, moves| {
                        anyhow::bail!(
                            "this add would move {} edits out of later changes, which the \
                             desktop can't confirm yet; run `wheat add --change` with --force",
                            moves.len()
                        )
                    },
                )?;
                Ok((
                    format!("Added to {}", short_change(&added.change.to_string())),
                    format!("{count} paths captured in “{}”.", added.title),
                ))
            }
            Request::Restore { paths } => {
                let tracked: Vec<String> = paths
                    .into_iter()
                    .filter(|path| !self.untracked.contains(path))
                    .collect();
                if tracked.is_empty() {
                    return Ok(("Nothing to restore".into(), "Untracked files stay.".into()));
                }
                // The window has no terminal to ask on, so `.git` paths are refused.
                let restored = authoring::restore(root, None, &tracked, DotGitGate::Refuse)?;
                Ok((
                    "Restored".into(),
                    format!("{restored} paths back to the materialized tree."),
                ))
            }
            Request::SelectChange { change } => {
                let title = authoring::select_change(root, None, parse_change(&change)?)?;
                Ok((
                    format!("Selected {}", short_change(&change)),
                    format!("`add` now goes to “{title}”."),
                ))
            }
            Request::SquashChange { change } => {
                let success = squash::execute(
                    root,
                    None,
                    squash::Scope::Change(Some(&change)),
                    Coauthors::Credit,
                )?;
                Ok((
                    format!("Squashed {}", short_change(&change)),
                    human(&success.human),
                ))
            }
            Request::SquashStack { .. } => {
                let success = squash::execute(root, None, squash::Scope::Stack, Coauthors::Credit)?;
                Ok(("Squashed stack".into(), human(&success.human)))
            }
            Request::Refresh | Request::SwitchFrontier { .. } => anyhow::bail!(
                "the desktop can't rematerialize yet; run `wheat refresh` or `wheat switch`"
            ),
            Request::Load
            | Request::WorkspaceDiff { .. }
            | Request::RevisionDiff { .. }
            | Request::Revisions { .. } => unreachable!("not an action"),
        }
    }
}

impl Backend for LiveBackend {
    fn handle(&mut self, request: Request) -> Vec<Update> {
        match request {
            Request::Load => vec![Update::Loaded(self.load().map_err(|e| format!("{e:#}")))],
            Request::WorkspaceDiff { path } => {
                let diff = self.workspace_diff(&path);
                vec![Update::WorkspaceDiff { path, diff }]
            }
            Request::RevisionDiff {
                change,
                revision,
                id,
            } => vec![Update::RevisionDiff {
                change,
                revision,
                diff: self.revision_diff(&id),
            }],
            Request::Revisions { change } => {
                let (revisions, complete) = match self.revisions(&change) {
                    Ok((revisions, complete)) => (Ok(revisions), complete),
                    Err(error) => (Err(format!("{error:#}")), false),
                };
                vec![Update::Revisions {
                    change,
                    revisions,
                    complete,
                }]
            }
            action => {
                let name = action_name(&action);
                let outcome = self.act(action).map_err(|error| format!("{error:#}"));
                vec![
                    Update::Done {
                        action: name,
                        outcome,
                    },
                    Update::Loaded(self.load().map_err(|e| format!("{e:#}"))),
                ]
            }
        }
    }
}

/// Creates the change a capture goes to, titled by the message's first line.
fn create(root: &Path, message: &str, base: NewBase) -> Result<ChangeId> {
    let (dependencies, independent) = match base {
        NewBase::Frontier => (Default::default(), false),
        NewBase::Changes(ids) => (
            ids.iter()
                .map(|id| parse_change(id))
                .collect::<Result<_>>()?,
            false,
        ),
        NewBase::Independent => (Default::default(), true),
    };
    let created = authoring::create_change(
        root,
        None,
        NewChange {
            dependencies,
            independent,
            metadata: MetadataArgs {
                message: message.to_owned(),
                description: None,
                created_at: None,
                authored_at: None,
                committed_at: None,
            },
        },
        |_| Ok(RevisionEdits::default()),
    )?;
    Ok(created.change)
}

fn parse_change(id: &str) -> Result<ChangeId> {
    ChangeId::from_str(id).map_err(|_| anyhow::anyhow!("invalid change ID {id}"))
}

fn human(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_owned()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
        })
}

/// How long ago `at` was, in its largest whole unit: `now`, `4m`, `3h`, `2d`.
fn age(at: Timestamp, now: i64) -> String {
    let seconds = now.saturating_sub(at.seconds).max(0);
    match seconds {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", seconds / 60),
        3_600..86_400 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

/// A content-addressed id's first eight characters, as the lists show it.
fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A change id's last eight hex digits. Change ids are time-ordered UUIDs, so
/// their first digits are the same for every change made around one time.
fn short_change(id: &str) -> String {
    let digits: Vec<char> = id.chars().filter(char::is_ascii_hexdigit).collect();
    digits[digits.len().saturating_sub(8)..].iter().collect()
}

fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `path`, with the home directory shown as `~`.
fn display_path(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return Path::new("~").join(rest).display().to_string();
    }
    path.display().to_string()
}

/// An untracked file, shown as all added when it is text.
fn untracked_diff(root: &Path, path: &str) -> Result<Diff> {
    if path.ends_with('/') {
        return Ok(Diff::Opaque {
            kind: "untracked directory".into(),
            facts: vec![("path".into(), path.to_owned())],
        });
    }
    let file = std::fs::File::open(root.join(path)).with_context(|| format!("open {path}"))?;
    let size = file.metadata()?.len();
    if size > MAX_UNTRACKED_BYTES {
        return Ok(Diff::Opaque {
            kind: "untracked".into(),
            facts: vec![("size".into(), format!("{size} bytes"))],
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    file.take(MAX_UNTRACKED_BYTES).read_to_end(&mut bytes)?;
    if text_capture::classify(&bytes) != text_capture::CaptureKind::Text {
        return Ok(Diff::Opaque {
            kind: "binary".into(),
            facts: vec![("size".into(), format!("{} bytes", bytes.len()))],
        });
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = vec![DiffLine {
        kind: LineKind::Hunk,
        old: None,
        new: None,
        text: format!("@@ -0,0 +1,{} @@", text.lines().count()),
    }];
    lines.extend(text.lines().zip(1u32..).map(|(line, number)| DiffLine {
        kind: LineKind::Added,
        old: None,
        new: Some(number),
        text: line.to_owned(),
    }));
    Ok(Diff::Text { lines })
}

/// The screens' diff of `document`. One file that a line diff can't show is
/// described instead; with several, each gets a header line.
fn document_diff(document: &DiffDocument) -> Diff {
    if let [file] = document.files.as_slice()
        && let Some(opaque) = opaque(file)
    {
        return opaque;
    }
    let several = document.files.len() > 1;
    let mut lines = Vec::new();
    for file in &document.files {
        if several {
            lines.push(DiffLine {
                kind: LineKind::Hunk,
                old: None,
                new: None,
                text: format!("{} · {}", file.path, change_word(file.change)),
            });
        }
        match &file.presentation {
            DiffPresentation::Text { diff }
            | DiffPresentation::Kind {
                diff: Some(diff), ..
            } => text_lines(diff, &mut lines),
            _ => {
                if !several {
                    continue;
                }
                if let Some(Diff::Opaque { kind, .. }) = opaque(file) {
                    lines.push(DiffLine {
                        kind: LineKind::Context,
                        old: None,
                        new: None,
                        text: kind,
                    });
                }
            }
        }
    }
    Diff::Text { lines }
}

fn text_lines(diff: &DiffText, out: &mut Vec<DiffLine>) {
    let number = |line: Option<usize>| line.and_then(|line| u32::try_from(line).ok());
    for hunk in &diff.hunks {
        out.push(DiffLine {
            kind: LineKind::Hunk,
            old: None,
            new: None,
            text: format!(
                "@@ -{},{} +{},{} @@",
                hunk.old_start, hunk.old_count, hunk.new_start, hunk.new_count
            ),
        });
        out.extend(hunk.lines.iter().map(|line| {
            DiffLine {
                kind: match line.operation {
                    DiffLineOperation::Context => LineKind::Context,
                    DiffLineOperation::Delete => LineKind::Removed,
                    DiffLineOperation::Insert => LineKind::Added,
                },
                old: number(line.old_line),
                new: number(line.new_line),
                text: diff
                    .text
                    .get(line.text_start..line.text_end)
                    .unwrap_or_default()
                    .trim_end_matches(['\n', '\r'])
                    .to_owned(),
            }
        }));
    }
}

/// What a file is when its presentation isn't lines of text.
fn opaque(file: &FileDiff) -> Option<Diff> {
    let size = |descriptor: Option<&wheat_core::diff_model::DiffDescriptor>| {
        descriptor.map_or_else(|| "—".to_owned(), |d| format!("{} bytes", d.bytes))
    };
    let kind = match &file.presentation {
        DiffPresentation::Text { .. } | DiffPresentation::Kind { diff: Some(_) } => return None,
        DiffPresentation::Binary => "binary".to_owned(),
        DiffPresentation::Symlink => "symlink".to_owned(),
        DiffPresentation::Kind { diff: None } => "type change".to_owned(),
        DiffPresentation::Omitted { reason } => format!("too large to show ({reason:?})"),
    };
    Some(Diff::Opaque {
        kind,
        facts: vec![
            ("path".into(), file.path.clone()),
            ("change".into(), change_word(file.change).into()),
            ("before".into(), size(file.old.as_ref())),
            ("after".into(), size(file.new.as_ref())),
        ],
    })
}

fn change_word(change: FileChangeKind) -> &'static str {
    match change {
        FileChangeKind::Added => "added",
        FileChangeKind::Deleted => "deleted",
        FileChangeKind::Modified => "modified",
        FileChangeKind::KindChanged => "type changed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wheat_client::authoring_metadata::{self, ConfigKey};

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        backend: LiveBackend,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("workspace");
            let store = dir.path().join("store");
            workspace_lifecycle::init(&root, Some(&store)).unwrap();
            authoring_metadata::set(Some(&store), false, ConfigKey::ActorName, "Test").unwrap();
            authoring_metadata::set(
                Some(&store),
                false,
                ConfigKey::ActorEmail,
                "test@example.invalid",
            )
            .unwrap();
            let backend = LiveBackend::new(root.clone());
            Self {
                _dir: dir,
                root,
                backend,
            }
        }

        fn write(&self, path: &str, contents: &str) {
            std::fs::write(self.root.join(path), contents).unwrap();
        }

        fn load(&mut self) -> Model {
            match self.backend.handle(Request::Load).as_slice() {
                [Update::Loaded(Ok(model))] => model.clone(),
                other => panic!("expected a model, got {other:?}"),
            }
        }

        fn act(&mut self, request: Request) -> Result<(String, String), String> {
            match self.backend.handle(request).into_iter().next() {
                Some(Update::Done { outcome, .. }) => outcome,
                other => panic!("expected an action's outcome, got {other:?}"),
            }
        }

        fn add(&mut self, paths: &[&str], target: Target, message: &str) {
            self.act(Request::Add {
                paths: paths.iter().map(|path| (*path).to_owned()).collect(),
                target,
                message: message.into(),
            })
            .unwrap();
        }

        fn workspace_diff(&mut self, path: &str) -> Diff {
            match self
                .backend
                .handle(Request::WorkspaceDiff { path: path.into() })
                .as_slice()
            {
                [Update::WorkspaceDiff { diff, .. }] => diff.clone(),
                other => panic!("expected a diff, got {other:?}"),
            }
        }
    }

    /// The files the workspace screen lists by default: everything but ignored ones,
    /// such as the `.wheat/` control directory.
    fn listed(model: &Model) -> Vec<(&str, FileCode)> {
        model
            .workspace
            .files
            .iter()
            .filter(|file| file.group != FileGroup::Ignored)
            .map(|file| (file.path.as_str(), file.code))
            .collect()
    }

    fn texts(diff: &Diff, kind: LineKind) -> Vec<String> {
        match diff {
            Diff::Text { lines } => lines
                .iter()
                .filter(|line| line.kind == kind)
                .map(|line| line.text.clone())
                .collect(),
            other => panic!("expected lines, got {other:?}"),
        }
    }

    #[test]
    fn a_capture_into_a_new_change_shows_in_the_model_with_its_diffs() {
        let mut fixture = Fixture::new();
        fixture.write("a.txt", "one\n");
        let model = fixture.load();
        assert!(model.changes.is_empty());
        assert_eq!(model.workspace.label, "main");
        assert!(model.workspace.frontiers.is_empty());
        assert!(model.workspace.frontier.is_some());
        assert_eq!(listed(&model), [("a.txt", FileCode::Untracked)]);
        assert_eq!(
            texts(&fixture.workspace_diff("a.txt"), LineKind::Added),
            ["one"]
        );

        fixture.add(&["a.txt"], Target::New(NewBase::Frontier), "Add a");
        let model = fixture.load();
        assert_eq!(model.changes.len(), 1);
        let change = &model.changes[0];
        assert_eq!(change.title, "Add a");
        // `change new`, then the capture.
        assert_eq!(change.revision_count, 2);
        assert_eq!(change.revisions.len(), 1);
        assert!(!change.published);
        assert_eq!(model.workspace.selected, Some(0));
        assert_eq!(model.workspace.frontiers.len(), 1);
        assert!(model.workspace.frontier.is_some());
        assert_eq!(model.store.unpublished_changes, 1);
        assert!(listed(&model).is_empty());

        fixture.write("a.txt", "two\n");
        let model = fixture.load();
        assert_eq!(listed(&model), [("a.txt", FileCode::Modified)]);
        let diff = fixture.workspace_diff("a.txt");
        assert_eq!(texts(&diff, LineKind::Removed), ["one"]);
        assert_eq!(texts(&diff, LineKind::Added), ["two"]);

        let id = model.changes[0].id.clone();
        let (revisions, complete) = fixture.backend.revisions(&id).unwrap();
        assert!(complete);
        assert_eq!(revisions.len(), 2);
        assert_eq!(revisions[1].id, model.changes[0].revisions[0].id);
        assert_eq!(revisions[1].actor, "Test");
        let diff = fixture.backend.revision_diff(&revisions[1].id);
        assert_eq!(texts(&diff, LineKind::Added), ["one"]);
    }

    #[test]
    fn restore_puts_back_tracked_files_and_leaves_untracked_ones() {
        let mut fixture = Fixture::new();
        fixture.write("a.txt", "kept\n");
        fixture.add(&["a.txt"], Target::New(NewBase::Frontier), "Add a");
        fixture.write("a.txt", "scribbled\n");
        fixture.write("new.txt", "new\n");
        fixture.load();
        fixture
            .act(Request::Restore {
                paths: vec!["a.txt".into(), "new.txt".into()],
            })
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(fixture.root.join("a.txt")).unwrap(),
            "kept\n"
        );
        assert!(fixture.root.join("new.txt").exists());
    }

    #[test]
    fn selecting_a_change_sends_the_next_capture_to_it() {
        let mut fixture = Fixture::new();
        fixture.write("a.txt", "a\n");
        fixture.add(&["a.txt"], Target::New(NewBase::Frontier), "First");
        fixture.write("b.txt", "b\n");
        fixture.add(&["b.txt"], Target::New(NewBase::Independent), "Second");
        let model = fixture.load();
        let first = model
            .changes
            .iter()
            .find(|change| change.title == "First")
            .unwrap()
            .id
            .clone();
        fixture
            .act(Request::SelectChange {
                change: first.clone(),
            })
            .unwrap();
        let model = fixture.load();
        let selected = model.workspace.selected.unwrap();
        assert_eq!(model.changes[selected].id, first);
        let second = model
            .changes
            .iter()
            .find(|change| change.title == "Second")
            .unwrap();
        assert!(second.independent);
    }

    /// Change B creates the file and C rewrites its tenth line. Capturing another
    /// rewrite of that line into B would move C's edit, which the window can't
    /// confirm yet, so it is refused and nothing is published.
    #[test]
    fn a_capture_that_would_move_edits_is_refused() {
        let mut fixture = Fixture::new();
        let base: String = (1..=20).map(|n| format!("l{n}\n")).collect();
        fixture.write("file", &base);
        fixture.add(&["file"], Target::New(NewBase::Frontier), "B");
        fixture.write("file", &base.replace("l10\n", "C1\nC2\n"));
        fixture.add(&["file"], Target::New(NewBase::Frontier), "C");
        let model = fixture.load();
        let b = model
            .changes
            .iter()
            .find(|c| c.title == "B")
            .unwrap()
            .id
            .clone();
        let before: Vec<usize> = model.changes.iter().map(|c| c.revision_count).collect();

        fixture.write("file", &base.replace("l10\n", "B2\n"));
        let refused = fixture
            .act(Request::Add {
                paths: vec!["file".into()],
                target: Target::Change(b),
                message: "B2".into(),
            })
            .unwrap_err();
        assert!(refused.contains("--force"), "{refused}");
        let after: Vec<usize> = fixture
            .load()
            .changes
            .iter()
            .map(|c| c.revision_count)
            .collect();
        assert_eq!(after, before);
    }

    #[test]
    fn changes_made_together_get_distinct_short_ids() {
        let first = ChangeId::new().to_string();
        let second = ChangeId::new().to_string();
        assert_eq!(short_change(&first).len(), 8);
        assert_ne!(short_change(&first), short_change(&second));
        assert!(first.replace('-', "").ends_with(&short_change(&first)));
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        let at = |seconds| Timestamp {
            seconds,
            offset_minutes: 0,
        };
        assert_eq!(age(at(1_000), 1_030), "now");
        assert_eq!(age(at(1_000), 1_000 + 4 * 60), "4m");
        assert_eq!(age(at(0), 3 * 3_600 + 5), "3h");
        assert_eq!(age(at(0), 2 * 86_400), "2d");
        assert_eq!(age(at(5_000), 1_000), "now", "a clock behind the commit");
    }
}
