# Wheat path history — Design

Status: decided (option 1, a local SQLite index), not yet implemented
Owner: bart
Last updated: 2026-11-06

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

- [x] **Decision — where the index lives (2026-11-06).** Bart: option 1, a local
  SQLite index in each store. See [Options](#options) and [Design](#design).
- [x] **Decision — what a directory's history is (2026-11-06).** Every change that
  changes anything below it: each change indexes its changed files and their
  ancestor directories (not the root, whose history is the plain history).
- [x] **Decision — who keeps it (2026-11-06).** Every store, clients included.

### Outstanding

- [ ] **`PathHistoryIndex`** in wheat-store: the SQLite file, catching up to a view
  on query (no writer hooks), and the candidate scan with its membership check.
- [ ] **`RepositoryBrowser::path_history(view, path, before, limit)`.** Newest
  first, a page of the changes that touch `path`, with a cursor.
- [ ] **Callers.** The file in each client store (`LocalRepository`), and in gitd
  where it opens a network; `catch_up` after a publish or import so a request never
  pays for a large backlog.
- [ ] **Benchmark.** A `wheat-bench` case at Gothab's scale (20,000 linear changes
  to 1,000 files in 100 directories): a sparse file's whole history and a
  directory's first page, with a contract on objects read per hit.
- [ ] **After the index, measured first:** metadata only for hits (Gothab's (b)), a
  decoded-object cache in `Network` (a), cheaper `application_at` and tips (d).

## Why this exists

Gothab's page fails a request over 1 s. At 20,000 linear changes to 1,000 files in
100 directories (gitd at v0.3.1, release, jemalloc), the whole history of a file
changed every 1,000 changes takes 1.78 s, and the first 30-entry page of a
directory changed every 100 takes 218 ms. gitd walks `browser.history` newest
first and, for each change, reads tree nodes along the path until a directory ID
matches the newer tree's: about 90 µs per change passed, whether it touches the
path or not (perf: malloc/free ~45%, blake3 ~15%, zstd ~10%, segment lookup). The
cost follows the changes passed, not the hits. A cache can't fix it: the first,
cold lookup has to fit the budget too (Bart, via @wise-walrus).

The fix is an index whose lookup costs O(log n + hits): for a path, the
applications that changed it, newest first.

## Goals

- A path's or directory's history page costs O(log n) object or row reads to find
  its first entry and O(1) per entry, cold, at any history length.
- Keeping the index costs, per change landed, O(changed paths × depth × log n).
- The same answer `gitd`'s walk gives today, for files and directories.

## Non-goals (v1)

- Renames and copies: a path's history ends where the path appeared.
- History across views in one query; a query is about one view.

## What a change touches

Each landing site already has the changed paths in hand (no extra reads):
`commit_writer` holds every edit's path with its before and after values, and
`repository_mutation::apply` computes `tree_edits`, the paths whose value changed.
The final tree update refuses implicit subtree removal, so those sets are exactly
the files that changed. A view that arrives by fetch or publish is validated
against its parent's tree diff (`content_map_follows_tree_diff`), which yields the
same set.

## Options

Positions belong to a view: a change's position is its index in the canonical
order of the view it was applied in (`ApplicationOrder::position`). An index entry
is (path key, position, application); a path key is a digest of the path, since
radix keys are fixed-length (the same reason `directory_name_key` hashes names).

1. **A local SQLite index in each store**, like the frontier index
   (`frontiers.sqlite`): rows (path key, position, application, change). A query
   scans one path's rows newest first and keeps those in the view: application
   `a` at position `p` is in the view when the view's application map maps its
   change to `a`, O(log n) per candidate. A store shared by a fork network holds
   every fork's rows, so a path's candidates include other forks' changes, which
   the membership check drops. Kept wherever applications are written or
   arrive; built for existing stores by one walk when first opened. Changes no
   object format. It is a cache: lost, it is built again. On a store whose objects
   live in S3 it lives on the server's local disk with the packed catalog.
2. **A content-addressed path-history map per view, kept beside the view, not in
   it**: a radix map like the history-by-time map (`history_map.rs`), keyed (path
   key, inverted position) → application, written as a new object kind and found
   through a store-local table from `ViewId` to its root. Each view's map is its
   parent's plus the change's keys, so views share nearly all of it, and no
   membership check is needed. Not in `ViewV1`, so view IDs and the wire format
   stay as they are; it is not sent to clients. Lives wherever the store's objects
   do. Existing views get a map by one walk.
3. **The map inside the view (`ViewV2`).** As 2, but referenced by the view, so it
   is validated, sent to clients, and every client can answer path history
   locally. The schema was locked at v0.3.0, so this is a new view tag with readers
   for both, and repositories written before it never get the index: their view IDs
   cannot change in place.

## Design

Like the frontier index (`frontiers.sqlite`), no writer knows about the index: a
query first brings it up to the view it asks about.

```sql
application(id INTEGER PRIMARY KEY, application BLOB UNIQUE, change BLOB, position INT)
path(id INTEGER PRIMARY KEY, path TEXT UNIQUE)
touch(path INT, position INT, application INT, PRIMARY KEY (path, position, application))
```

**Catching up to view `V`.** From `V`, follow each view's producing application
(`ApplicationMapStore::get(view.applications, view.order.last)`) to its target view,
newest first, until a view whose producing application is in `application` (or the
base view). For each view passed, the changed paths are the tree diff from its
parent's tree to its own (`MerkleTreeStore::diff_cursor`), each file plus its
ancestor directories. Everything passed is inserted oldest first in one transaction,
so an indexed application's ancestors are always indexed. Cost: the views passed and
their diffs, once; a query after a publish of n changes pays for those n.

**A page.** Scan `touch` for the path newest first below the cursor position. Keep
each candidate whose application is in `V`: `V`'s application map gives its change's
application, O(log n) node reads. In a fork network the store holds every fork's
rows, and candidates from other forks are dropped here. Read the history item only
for the rows kept.

**Trust.** Every row comes from views in the local store, which were validated
when they were written or arrived; the index is a cache, rebuilt from them when its
file is missing or unreadable.

## Open questions for review

None open.

## References

- Gothab: `crates/gothab-gitd/tests/wheat_history_scale.rs` (the benchmark),
  `crates/gothab-gitd/src/wheat/history.rs` (`touching()`, today's walk).
- `docs/design/wheat-frontier-index.md`: the local SQLite index precedent.
- `crates/wheat-store/src/history_map.rs`, `radix.rs`: the history-by-time map.
- `docs/design/server-native-source-control-store.md`: the schema lock (v0.3.0).
