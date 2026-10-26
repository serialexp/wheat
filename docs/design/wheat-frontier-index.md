# Wheat frontier index — Design

Status: complete (2026-10-27): option 1, a local SQLite index holding every
descendant of each label's view, with today's rule for what counts as a label's
frontier
Owner: bart
Last updated: 2026-10-27 (complete)

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

- [x] **Decision — the shape of the index (2026-10-27).** Option 1, the local SQLite
  index. Bart: the store already keeps SQLite as a cache of what comes from
  elsewhere (the packed catalog), so this is the same kind of thing. It changes no
  object format, so it is off the V1 list. See [Decision](#decision).
- [x] **Decision — what a label's frontiers are (2026-10-27).** Today's rule: every
  tip below the label's view, including work under other labels. Bart: "a branch
  based on a different label is still a frontier of main as well"; integration may
  treat them differently later.
- [x] **Decision — what the index keeps per view (2026-10-27).** Every descendant and
  which are tips (Bart chose this over tips only).
- [x] **Implementation.** `crates/wheat-store/src/frontier_index.rs`
  (`FrontierIndex`, SQL in `frontier_index_sql/`), the file `frontiers.sqlite` in
  each store (`LocalRepository::frontiers`). Every client caller goes through
  `frontier_service::LabelFrontiers`: switch, refresh, clone, init, `frontier list`,
  `label move`, publish, fetch's hidden-work check and integrate. Instead of a hook
  at every lineage writer, a query brings the index to the lineage it is asked about
  with `ViewLineageMapStore::growth`, the edges added between two roots, so no writer
  has to know about it.
- [x] **Tests.** `crates/wheat-store/tests/frontier_index.rs`: the index matches the
  walk after every step of randomly grown lineages (new views, joins, edges between
  existing views including cycles, batches, labels moving anywhere, rewrites that drop
  edges), a query after one new edge reads at most 24 objects over a 400-view chain,
  the file outlives the process and a damaged one starts empty, views no label points
  at are dropped, and the walk's limits hold. Two deliberate bugs (tips not cleared,
  descendants of a newly reached view not followed) each fail the random test.
- [x] **Benchmark case `label_frontiers`.** A label left at the base under N creates.
  Release, 2026-10-27: the warm index reads 7 objects at 1,000, 10,000 and 30,000
  changes; the walk (and the index's first build) 1,243, 10,955 and 35,358 objects,
  0.23 s at 30,000. Contract: at most 16 reads at 30,000, and growth per bit of
  history at most 125%. Amends are not benchmarked separately: the index's cost for
  any mutation is the lineage diff it made, which an amend widens by the views it
  recomposes, and the random test covers the shapes amends and integrations add.
- [x] **The walk decodes each map node once (found by the benchmark).** At 30,000
  views below a label the walk failed with "view-lineage decoded-byte limit exceeded
  (256 MiB)": each view's lookup decoded the map's upper levels again, about 9 KB a
  view. So `switch`/`refresh`/`label move` on a label with about 28,000 views below
  it, such as an old tag in a large repository, failed, and so would the index's
  first build. The walk now keeps the map nodes it decoded
  (`Budget::decoded_map_nodes`), so it costs at most the map plus a child set per
  view. Test: `a_walk_reads_each_map_node_once` (`tests/view_lineage.rs`; 2,009 reads
  for a 2,000-view chain, over 4,000 without the cache).
- [x] **Closed the TODO.md item** "`descendant_frontiers` walks every view since the
  label basis".

### Outstanding

_(nothing)_

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

## Decision

Option 1, keeping every descendant per indexed view (2026-10-27).

- **Rows.** `anchor(view, descendants)` for each view the index holds,
  `descendant(anchor, view, tip)` for each view below it, and `stamp(lineage,
  generation)`: the lineage root the rows describe.
- **Query.** `FrontierIndex::frontiers(objects, lineage, label views, basis, limits)`
  brings the index to `lineage`, drops anchors no label points at (the queried basis
  is kept), and answers from the `tip` rows, building `basis`'s rows with one walk
  when it is not held yet.
- **Bringing it up to date.** `growth(stamp, lineage)` lists the edges added. For each
  anchor a parent is below, or is, the new children and whatever below them the
  anchor does not hold yet are added, each with whether it has children; parents lose
  their tip flag. When the later lineage drops an edge (squash, GC), or the stamp's
  lineage cannot be read, the index is emptied and anchors are rebuilt as they are
  asked for. A query against a lineage older than the stamp (another process behind
  this one) counts as dropped edges: correct, at the cost of one rebuild.
- **Several processes.** Every write runs in an immediate transaction that checks
  `generation` is what the writer read and bumps it; a writer that lost the race
  starts over, and after four tries answers with the walk without writing.
- **Cost.** A warm query reads the lineage diff since the last query and one child
  lookup per new view per anchor above it. Storage and per-edge work grow with the
  views under each held anchor; a tag on an old view that someone queried holds
  most of the history until the tag moves or goes.

## Open questions for review

- [x] **Which option.** Option 1 (2026-10-27), above.
- [x] **Do a label's frontiers include work under another label above it?** Yes,
  today's rule stays (2026-10-27).

## References

- `TODO.md`, "`descendant_frontiers` walks every view since the label basis".
- `TODO_DONE.md`, "`open` walks every view since the basis on each mutation" (the
  application shortcut).
- `docs/design/wheat-label-moves.md`, Follow-ups.
