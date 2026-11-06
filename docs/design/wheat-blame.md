# Wheat blame — Design

Status: partial — `blame`, `RepositoryBrowser::blame`, the benchmark and `wheat blame PATH` landed; the tag for Gothab outstanding
Owner: bart
Last updated: 2026-11-06

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

- [x] **Decision — the line diff (2026-11-06).** Bart: histogram, through
  `imara-diff`, with its line post-processing (git's indent heuristic).
- [x] **Decision — at the bound (2026-11-06).** Bart: stop and report `Unresolved`
  ranges.
- [x] **Decision — a client command (2026-11-06).** Bart: `wheat blame PATH
  [--limit N]`.
- [x] **`wheat_store::blame`** (`blame.rs`) and **`RepositoryBrowser::blame`**. Tests
  (`tests/blame.rs`) check every line of a forked random history's files against the
  change the test wrote it in, carry an unresolved range on from its `before` view,
  stop at a version that was not text, refuse what a blame cannot read, and count a
  blame's object reads. Origins are `BlameChange` (the touch and its application
  record) from the walk, `HistoryItem` from the browser.
- [x] **Benchmark.** `wheat-bench` case `blame`: 20,000 linear changes, every 20th
  editing 5 lines of a 2,000-line file, each blame checked line by line. Release,
  2026-11-06: the hot file's blame walks its 1,000 changes (759 origins) in 223 ms and
  10,893 reads, a file changed 20 times 0.6 ms and 78 reads. Contracts: reads per
  change walked and per origin, and Gothab's 1 s page.
- [x] **Clients: `wheat blame PATH [--limit N]`** (`blame_inspection.rs`): lines
  grouped under the change that wrote them; JSON gives ranges, origins and whether the
  walk resolved every line. It blames the effective view, not uncommitted edits.

### Outstanding

- [ ] **API sent to Gothab** (@wise-walrus), and a tag when it lands.

## Why this exists

Gothab's file page shows, for each line, the change that last wrote it. Gothab
can't build that over Wheat's public reads at its 1 s page budget: it would walk the
view's whole history. Blame belongs beside `path_history`, which already answers
"which changes touched this file" in O(log n + hits) (`docs/design/wheat-path-history.md`).

## Goals

- For one file at one view: its lines as ranges, each with the change (and its
  history item) that last wrote it.
- Cost follows the changes that touched the file, not the view's history: one tree
  lookup, one content read and one line diff per touching change walked.
- Bounded: a blame walks at most `max_changes` touching changes and reads files of at
  most `max_bytes`, and says what it left unresolved.
- Deterministic: the same answer on every machine, whatever its load.

## Non-goals (v1)

- Renames and copies, and lines moved between files: a line's origin is in this path's
  history (as `path_history`'s is).
- Detecting moved lines within a file, ignoring whitespace, or skipping chosen changes
  (git's `-M`, `-w`, `--ignore-rev`).
- A stored blame index: computed per request from the path history index.

## The walk

History is linear per view, so blame is the classic walk.

1. Read the file at `view` (refused when missing, longer than `max_bytes`, or not
   text by `text_capture::classify`). Split it into lines, each with its terminator;
   every line is unresolved and knows its line in the version being walked.
2. Take the file's touching changes newest first from `path_history` in pages. For
   each, the version before it is the file in the change's parent view's tree. The
   version after it is the one the previous step held (the same content ID: checked),
   so each version is read once.
3. Diff before → after by lines. Unresolved lines the diff inserts belong to this
   change; the rest move to their line in the before version.
4. Stop when no line is unresolved, when the before version has no file (the change
   created it: everything left is its), when the before version is binary or too
   long (everything left belongs to the change that made it text), or at
   `max_changes`.

Adjacent lines with the same origin merge into one range.

Cost per change walked: a few tree node reads for the path in the parent's tree,
one view record, the content (decompressed), and the diff. The content of a
version is read once, so a blame reads each version of the file once.

## API

```rust
pub struct BlameLimits {
    /// The longest version of the file read; longer ones end the walk.
    pub max_bytes: usize,
    /// The most touching changes walked.
    pub max_changes: NonZeroUsize,
}

pub struct Blame {
    /// The file's lines at the view.
    pub lines: u32,
    /// Ranges in line order that cover every line.
    pub ranges: Vec<BlameRange>,
    /// The changes the ranges name, newest first.
    pub origins: Vec<HistoryItem>,
}

pub struct BlameRange {
    /// The first line (0-based) and how many.
    pub start: u32,
    pub len: u32,
    pub origin: BlameOrigin,
}

pub enum BlameOrigin {
    /// Written by `origins[index]`.
    Change { index: usize },
    /// Older than every change walked (the walk stopped at its bound): these lines
    /// are lines `line..line + len` of the file in view `before`.
    Unresolved { before: ViewId, line: u32 },
}

impl RepositoryBrowser<'_, O> {
    pub async fn blame(
        &self,
        index: &PathHistoryIndex,
        view: ViewId,
        path: &str,
        limits: BlameLimits,
    ) -> Result<Option<Blame>, BrowseError>; // None when no file is at `path`
}
```

As built, `RepositoryBrowser::blame` returns `Blame<HistoryItem>`, and the walk itself
is `wheat_store::blame(reader, index, &ViewRecord, path, limits) ->
Option<Blame<BlameChange>>` for callers that hold a view record rather than a
browser (the CLI). Refusals are `BlameError::{NotText, TooLong { size, max },
InvalidPath}`, inside `BrowseError::Blame` from the browser. The diff's own
heuristics bound its work, so no separate line budget was needed.

`Unresolved` gives Gothab "blame prior": a blame of `path` at `before`, whose line
`line` is this range's first.

## Options

1. **The line diff.**
   - (a) **Myers, through `similar`**, which `wheat diff` already uses. Its worst case
     is O(N·D): two unrelated 20,000-line versions are a large cost. `wheat diff`
     bounds it with a deadline, which is time and so differs by machine; blame would
     bound the work by lines instead (a change whose diff exceeds it counts as
     rewriting every unresolved line it touches).
   - (b) **Histogram, through `imara-diff`** (git's `--diff-algorithm=histogram`,
     gix's choice): usually faster than Myers and better on unrelated regions, a new
     direct dependency (gix brings its fork, `gix-imara-diff`). Same work bound.
   - Both give the same answer on every machine.
2. **At the bound.**
   - (a) **Stop and report `Unresolved` ranges** (the API above). A page answers
     within its bound; Gothab links "older" lines to a blame at `before`.
   - (b) **Stop and return a cursor** that carries the unresolved lines' positions, so
     the next call continues the same walk. Exact, but the cursor grows with the file's
     lines and Gothab has to hold or send it.
3. **A client command.** `wheat blame PATH [--limit N]` over the same call, as `wheat
   log PATH` uses `path_history`, or nothing on clients for now.

## Open questions for review

None open; the three in [Options](#options) were decided on 2026-11-06.

## References

- `docs/design/wheat-path-history.md`: the index blame walks.
- `crates/wheat-store/src/browse.rs`: `RepositoryBrowser`.
- `crates/wheat-client/src/workspace_diff.rs`: `wheat diff`'s line diff and its budget.
- `crates/wheat-core/src/text_capture.rs`: text classification.
