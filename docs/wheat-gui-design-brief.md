# Design brief: a desktop client for Wheat

For the Forge Design System, to be built with wgpu-gameui like agent-ui. It's meant to be
what TortoiseSVN or SourceTree is for their systems: a window onto a repository where the
everyday work (looking at what changed, capturing it, moving between versions, resolving
conflicts, syncing with Git) takes a few clicks instead of CLI flags.

Wheat is not Git, and most of this brief explains why. A design that reuses SourceTree's
screens with the words changed will draw the wrong thing. Please read "How Wheat differs"
before the screens.

## How Wheat differs from Git

- **A change, not a commit, is the unit of work.** A change has a stable identity and a title
  and lives for as long as the work does. Each time you capture more work into it, it gets a
  new immutable **revision**. So a change has its own history: revision 1, 2, 3 of the same
  piece of work. This is close to a Gerrit change with patch sets, or a jj change. The main
  history view is therefore two-dimensional: changes and how they depend on each other, and
  inside each change its revisions.
- **Changes depend on changes.** A change is built on the current frontier, on named other
  changes, or independently on the repository base. When you amend a change, the changes that
  depend on it are reapplied automatically. A reapplication that no longer fits becomes a
  conflict on that dependent change. One edit can therefore change the state of several rows
  at once, and the design has to make that visible.
- **There's no staging area.** You pick paths and "add" them, and that creates the next
  revision of the selected change straight away. Every workspace has exactly one **selected
  change** that `add` goes to, unless you name another one.
- **Views are the trees you check out.** A view is an immutable tree composed of applied
  changes, identified by its content. A **label** is a short name bound to a view, like a
  branch name, but rebinding it is an explicit act. Imported labels remember the Git ref they
  came from.
- **Frontiers.** Below a label there can be several descendant views: the label plus
  different sets of later work. A workspace selects one. The rule is zero, one or many: with
  one, it's chosen for you. With several, the user picks one or **integrates** them, which
  joins them into one view. The client should make "there are 3 frontiers here" visible, not
  hide it.
- **Conflicts are data, not a broken working copy.** A conflict is recorded in the repository
  with evidence for each edit (which edit failed to apply, and where). It persists, can be
  inspected from any workspace, and is resolved per path by choosing an exact result. The
  working copy is never left half-merged with conflict markers.
- **Several workspaces share one store.** Each AI agent works in its own workspace (that's
  agent-ui's main use case), and so may a person. Each workspace has its own label, frontier,
  selected change and local edits. Seeing all of them at once is something no Git client
  offers, and it's probably this client's best reason to exist.
- **Other agents move the repository while you look at it.** Every write is compare-and-swap.
  When it fails because something else moved first, the result is a retryable "stale" error,
  not data loss. `status` also reports "repository advanced since materialization". Both need a
  calm, routine treatment, such as a refresh affordance, rather than an error dialog.
- **Git is a bridge, not the home.** Wheat imports a Git repository, pushes a view to a Git
  branch, and pulls new Git commits in, moving local work above them. It also has its own
  remote protocol (clone, fetch, publish to a wheat-server). Both kinds of remote appear in
  the UI.
- **Squash.** Unpublished revisions of one change, or of every change in a stack, can be
  collapsed into one. Published ones can't.

## What it needs to show

Roughly in priority order. Everything listed exists in the CLI today unless marked **(new)**.

### 1. Workspace (the home screen)

- Which label, frontier and selected change this workspace is on, with a warning when the
  repository has moved since it was materialized.
- Tracked changes compared with the materialized tree: modified, deleted, and type changed
  (file / executable / symlink). Then untracked files, and ignored files behind a toggle.
- A diff of the selected file.
- Capture: pick files (or "all tracked"), write a message, and add them to the selected
  change, to another existing change, or to a new change. A new change can depend on the
  frontier, on chosen changes, or on nothing. The first line of the message becomes the
  change's title.
- Restore: discard local edits to selected paths, back to the materialized tree.

### 2. Changes and revisions (the history screen)

- Changes as rows, with their dependency graph (a DAG, but usually stacks). Each row shows
  the title, author, time, number of revisions, published or not, and conflicted or not.
- Expanding a change shows its revisions, each with message, actor and timestamp.
- Selecting a revision shows its diff against its predecessor. **(new)** Comparing two
  revisions of the same change (an interdiff) would be the natural next ask.
- Actions: select as the active change, squash a change or a stack, and push to Git.
- Scale: a repository imported from Git can have thousands of changes. The list has to
  virtualise and find things quickly.

### 3. Labels, views and frontiers

- A list of labels, each with its view, its frontier count, and the Git ref it was imported
  from, if any. Create, rename and delete. The active label can't be deleted or renamed.
- The frontiers below a label. Switch to one, or integrate several.
- A diff between any two views.

### 4. Conflicts

- The list of conflicts, with the affected paths and, for each, which edit failed and the
  evidence for it.
- Resolving a path by choosing a result: one side, the other, or an edited file. Once
  resolved, it leaves the list.
- Entry points from the change rows and from the workspace, because a conflict usually
  appears as a side effect of something else (an amend, an integrate, a pull).

### 5. Workspaces and agents

- Every workspace attached to the store: path, label, frontier, selected change, and
  whether it has unpublished or local work. Agent workspaces should be recognisable as such.
- Opening another workspace's selected change, to see what an agent is working on. Detaching
  a workspace.

### 6. Remotes and sync

- Git: import (first time), pull and push to a branch. Show what a push will send and how
  much of it is new. Push and pull report counts already.
- Wheat server: clone, fetch, publish.
- Progress and results for these long operations, which can take seconds on big
  repositories.

### 7. Maintenance (low priority)

- Validate the repository, repack, storage statistics, and configuration (actor name and
  email, ignore rules).

## States and edge cases to design

- A clean workspace; one with hundreds of changed paths; one where the repository has moved.
- Several frontiers below the active label (the "you must choose" state).
- A change whose amend has just put conflicts on three dependent changes.
- A stale write (someone else moved first): a retry, not an error.
- Diffs that are binary, symlink, too large, or too complex; Wheat reports these as opaque on
  purpose, with metadata only.
- An empty repository straight after `init`, and the first import from Git.
- A 2,000-change imported history.

## Data the client will receive

Every command has a versioned JSON output (`--format json`), with stable error categories,
symbolic error codes, a retryable flag, and exit classes. Whether the client runs the CLI or
links the Wheat crates directly is an engineering decision; it doesn't change the screens.
Identities are long content hashes (change, revision, view, edit). Show short forms, and
never make the user type them; the CLI is built so routine work needs no copied IDs.

## Existing pieces to reuse

From agent-ui: the sidebar with two-line rows, status dots and chips, the transcript's diff
and line-numbered output wells, Table, tabs, popovers, floating windows, the menu bar and the
status bar. A DAG or stack graph next to a list, a split diff, and a conflict resolution view
are probably new components.
