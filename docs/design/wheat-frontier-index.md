# Wheat frontier index — Design

Status: proposed, awaiting Bart's choice of shape (the question timed out on
2026-10-26; nothing is implemented)
Owner: bart
Last updated: 2026-10-26

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

_(nothing yet)_

### Outstanding

- [ ] **Decision — the shape of the index.** One of options 1–4 below. Options 2–4
  change the object format and so must land before V1; option 1 does not.
- [ ] **Implement the chosen option**, with maintenance at every lineage writer
  (mutation, integration, Git import, union, rewrite, repack, hydration).
- [ ] **Benchmark case.** A label left in place under N mutations (including amends
  of a change the label holds), counting the objects `frontier list`, `refresh` and
  `label move` read, with a contract that they do not grow with N.
- [ ] **Close the TODO.md item** "`descendant_frontiers` walks every view since the
  label basis".

## Why this exists

`ViewLineageMapStore::descendant_frontiers(root, basis)`
(`crates/wheat-store/src/view_lineage.rs`) finds the leaves of the view lineage
under a label's view by walking forward from it. The lineage stores only
parent → children links, so the walk visits every view created under the label: each
mutation adds at least one view, and a composition that rebuilds from below adds one
per change it recomposes. Switch, refresh, clone, init, `frontier list`, and since the
label moves `publish`, `fetch`, `integrate` and `label move` all call it. Moving the
label on publish keeps the distance short in normal use; a label left in place while
work piles up on it does not.

The mutation path had the same walk (`is_descendant`) and was fixed with the
application shortcut (`PersistedRepositoryMutator::descends`): the effective view's
application at the basis's size targets the basis, found in O(log n) reads. That proves
*one* ancestry; it does not enumerate the leaves.

## Facts the options rest on

- **Only the client asks.** Every caller is in `wheat-client` (`frontier_service.rs`,
  `label_move.rs`) or tests. The server never computes frontiers, so they cross no
  trust boundary today.
- **Every query starts at a label's view** (the label's basis in `frontier_service`,
  the `from` view in `label_move::check`, a label's old view in `hidden_by_fetch`).
- **The lineage is a DAG.** Integration adds an edge from the basis and from every
  frontier to the result (`repository_integration.rs`). Composition adds an edge per
  recomposed step, starting from the view below the first change that differs, which
  can be below the label's view when a change the label holds is revised; the
  continuation edge from the old effective view joins the new result back
  (`ensure_continuation_edge`). So after an amend of a published change, the new
  frontier is not composed through the label's view, and the application shortcut
  does not prove it descends.
- **Lineage writers:** mutation (`insert_edges` of `fresh.view_edges`), Git import
  finalize, union on fetch (`ViewLineageMapStore::union`), rewrite/squash (rebuilds the
  lineage over the retained views, which can turn a view back into a leaf), repack and
  hydration. Every option has to be kept right at each of them.
- **`weight`** on a view's order strictly increases along every lineage edge
  (checked by validation), so it bounds any ancestry search.

## Options

### 1. Local SQLite index (no format change)

A table in the store's local SQLite (next to the catalog) maps each label's view to its
descendant leaves, stamped with the lineage root it is valid for. An operation that
adds edges updates it from those edges: a leaf that gains a child is replaced by the
child, and a new child of a label's view is added to that view's leaves and those of
the labels above it. When the stamp does not match the head's lineage (a fetch, union or
rewrite that does not hand over its delta, or a missing table), it is rebuilt with one
forward walk per label, as today.

- Queries: O(frontiers).
- Format: none, so it is not a V1 blocker.
- Cost: the first query after a stale stamp is the walk of today. It is a second
  source of truth that must agree with the lineage; a bug shows up as wrong frontiers
  until the next rebuild, so it needs a check (debug assertion or `fsck`) against the
  walk.

### 2. Leaves set in the state (format change)

`RepositoryStateV1` gains `lineage_leaves`, the set of views with no children. Each
edge costs an O(log n) insert and delete. Validation checks each edge's parent is not
a leaf and each new leaf is, incrementally. A query reads the leaves, drops those
whose weight is not above the label's view, and tests each remaining one with the
application shortcut; when that fails it falls back to the forward walk.

- Queries: O(leaves × log n) when every frontier was composed through the label's
  view; the forward walk otherwise, which is the case right after an amend of a
  change the label holds.
- Format: one field in the state, validated.

### 3. Leaves set and parent links (format change)

Option 2 plus a child → parents map in the state. The ancestry test walks back from
each leaf, bounded by weight, trying the application shortcut at each view, so it only
steps across continuation and integration edges.

- Queries: O(leaves × (rebuilds since the label + 1) × log n).
- Format: two structures in the state; every edge is written twice; validation checks
  the two maps are inverses.

### 4. Per-label frontier list in the label manifest (format change)

`ViewLabelManifestEntryV1` gains `frontiers`. The manifest is published, so a
receiver has to check the list against the lineage, which is the forward walk this is
meant to avoid, and a fetch has to merge the remote's lists with the local ones over
the union lineage.

- Queries: O(frontiers).
- Format: a manifest field, plus a validation cost of today's walk.

## Open questions for review

- [ ] **Which option.** The question was put to Bart on 2026-10-26 and timed out.
- [ ] **Do a label's frontiers include work under another label above it?** Today they
  do: a label at an ancestor of another label's view sees the other label's leaves.
  Options 1 and 4 could instead track only work done on the label itself, which
  changes what `label move` reports as left behind.

## References

- `TODO.md`, "`descendant_frontiers` walks every view since the label basis".
- `TODO_DONE.md`, "`open` walks every view since the basis on each mutation" (the
  application shortcut).
- `docs/design/wheat-label-moves.md`, Follow-ups.
