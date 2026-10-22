//! What the screens draw, independent of where it came from: the live store
//! through wheat-client, or the comp's demo data in `fixture`.

/// One repository: the store, the workspaces attached to it, and the one this
/// window is on.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// The repository's display name, e.g. the workspace directory's.
    pub name: String,
    /// The workspace directory, shortened for display.
    pub path: String,
    pub workspaces: Vec<WorkspaceEntry>,
    pub store: StoreSummary,
    pub workspace: WorkspaceView,
    /// Changes, newest first.
    pub changes: Vec<Change>,
    /// How many changes the store has beyond `changes`, which may be a page.
    pub older_changes: usize,
}

/// A workspace attached to the store, for the sidebar.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceEntry {
    pub name: String,
    /// `main/f2`: the label and frontier it has materialized.
    pub frontier: String,
    /// The change `add` goes to, as an index into `Model::changes`.
    pub selected: Option<usize>,
    pub current: bool,
    pub agent: Option<Agent>,
    pub state: WorkspaceState,
    /// What it is doing, when something says so.
    pub task: Option<String>,
}

/// An AI agent's workspace: its tint in the sidebar.
#[derive(Clone, Copy, Debug)]
pub struct Agent {
    pub hue: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkspaceState {
    #[default]
    Idle,
    Running,
    Waiting,
}

#[derive(Clone, Debug, Default)]
pub struct StoreSummary {
    pub changes: usize,
    pub unpublished_changes: usize,
    pub unpublished_revisions: usize,
    pub conflicted_changes: usize,
    pub conflicted_paths: usize,
}

/// This window's workspace: where it stands and what changed in it.
#[derive(Clone, Debug, Default)]
pub struct WorkspaceView {
    pub label: String,
    /// The selected frontier, `None` when several wait for a choice.
    pub frontier: Option<String>,
    /// The frontiers below the label, for "1 of 3" and the choice banner.
    pub frontiers: Vec<FrontierChoice>,
    /// The selected change, as an index into `Model::changes`.
    pub selected: Option<usize>,
    /// Set when the repository moved since this workspace was materialized:
    /// who moved it, when, as one line.
    pub moved: Option<String>,
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug)]
pub struct FrontierChoice {
    pub name: String,
    pub changes: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGroup {
    Tracked,
    Untracked,
    Ignored,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileCode {
    Modified,
    Deleted,
    TypeChanged,
    Untracked,
    Ignored,
}

impl FileCode {
    pub fn letter(self) -> &'static str {
        match self {
            FileCode::Modified => "M",
            FileCode::Deleted => "D",
            FileCode::TypeChanged => "T",
            FileCode::Untracked => "?",
            FileCode::Ignored => "!",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: String,
    pub code: FileCode,
    pub group: FileGroup,
    /// `+38 −11`, `binary`, `file → exec`; empty when unknown.
    pub stat: String,
}

#[derive(Clone, Debug, Default)]
pub struct Change {
    pub id: String,
    pub short: String,
    pub title: String,
    pub author: String,
    pub agent: bool,
    /// `4m`, `2d`: how long ago its latest revision was made.
    pub age: String,
    /// Oldest first; `revisions.len()` is the revision count.
    pub revisions: Vec<Revision>,
    /// The changes it is built on, as indexes into `Model::changes`.
    pub depends_on: Vec<usize>,
    /// The changes built directly on it: the reverse of `depends_on`, filled
    /// in by `Model::index_dependents`.
    pub dependents: Vec<usize>,
    pub independent: bool,
    pub published: bool,
    pub conflicted: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Revision {
    pub message: String,
    pub actor: String,
    pub age: String,
}

/// A diff as the screens draw it.
#[derive(Clone, Debug)]
pub enum Diff {
    /// Nothing picked.
    None,
    Text {
        lines: Vec<DiffLine>,
    },
    /// Something a line diff can't show: binary, a type change, a deletion.
    Opaque {
        kind: String,
        facts: Vec<(String, String)>,
    },
    /// Being computed; drawn as an empty well.
    Loading,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Hunk,
    Context,
    Added,
    Removed,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: LineKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

impl Model {
    /// Fills every change's `dependents` from the others' `depends_on`. Once per
    /// model, so walking a stack costs only its own size.
    pub fn index_dependents(&mut self) {
        for change in &mut self.changes {
            change.dependents.clear();
        }
        for index in 0..self.changes.len() {
            for at in 0..self.changes[index].depends_on.len() {
                let base = self.changes[index].depends_on[at];
                if let Some(change) = self.changes.get_mut(base) {
                    change.dependents.push(index);
                }
            }
        }
    }

    /// The changes that depend on `index`, directly or not, nearest first.
    pub fn dependents(&self, index: usize) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut next = 0;
        let mut at = index;
        loop {
            for &dependent in &self.changes[at].dependents {
                if seen.insert(dependent) {
                    out.push(dependent);
                }
            }
            let Some(&following) = out.get(next) else {
                return out;
            };
            next += 1;
            at = following;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dependents_come_nearest_first_and_once() {
        // 3 ← 2 ← 0, 3 ← 1 ← 0: a diamond above 3.
        let mut model = Model {
            changes: [vec![1, 2], vec![3], vec![3], vec![]]
                .into_iter()
                .map(|depends_on| Change {
                    depends_on,
                    ..Change::default()
                })
                .collect(),
            ..Model::default()
        };
        model.index_dependents();
        assert_eq!(model.dependents(3), vec![1, 2, 0]);
        assert_eq!(model.dependents(0), Vec::<usize>::new());
    }
}
