# Wheat label moves — plan

Status: proposed (2026-10-15), revised after Codex reviews 1–3; references refreshed
2026-10-22 after the prerequisite validation project (decision 13) landed, then revised
after review 4 (Codex and Antigravity, 2026-10-22); step 1 in progress
Owner: bart

## Why

Labels never move today. `wheat label` can create, rename and delete; publish,
integrate and pull leave every label where it was. After a pull, `main` still names
the view it named before, and the integrated work is only a frontier below it. The
only way to move `main` is delete and create. The design docs defer "label
advancement and frontier consolidation" until a policy decision; this is that
decision.

## Bart's decisions (2026-10-15)

1. An explicit `wheat label move`, and `integrate`, `pull` and `publish` move the
   active label by default, with a way to say no (`--keep-label`).
2. Moves must be forward; anything else needs `--force`.
3. A move that would leave a frontier behind refuses without `--force`, for any
   frontier.
4. The server enforces forward moves on publish.
5. When publish cannot move the label, publish refuses as a whole.
6. "Forward" compares edits, not revision IDs: a revision with the same edits is the
   same content. A removal made on the new revision's side still counts as forward;
   ancestry is used only to tell a removal from an edit never seen.
7. A label moved differently on both sides of a fetch: integrate joins frontiers that
   lack some of the label's changes (rather than remote-tracking labels).
8. Revision merging uses an ancestry DAG: a combined revision records the revisions
   it combines. (Chosen over carrying removed-edit sets, after review 2.)
9. Changes the label holds can be revised: `wheat add`, `change select` and
   `stack squash` no longer refuse them. Squash still never collapses the basis's
   revision of a change or anything before it.
10. Annotated labels move under the same rule as any label.
11. `refresh`, when the workspace's view is not covered by the label's new view,
    integrates (it does what `integrate` does).
12. An edit counts as removed when a line that no longer has it removed an identical
    edit (same EditId) since the merge base, even if another line added its own copy
    independently (review 3, item 1). Merge and forward use the same rule.
13. Incremental view validation at publish and fetch is a prerequisite project with
    its own plan, landed first (review 3, items 4 and 8). This plan assumes it: views
    a store holds were checked when they arrived, so local commands may trust them.

## Bart's decisions (2026-10-22, after review 4)

14. Squash stops at branch points: it never collapses a revision another line
    descends from, so a squash cannot erase the removal evidence a later merge with
    that line needs (review 4, item 2).
15. Merge work in validation has its own meter, `MergeBudget`, separate from the
    prerequisite's `IncrementBudget` object-read meter; bootstrap and full validation
    run under it too (review 4, item 5).
16. `wheat label move` rebinds this workspace only when the target is its effective
    view; otherwise the workspace keeps its basis, observed binding and selection and
    shows drift until refresh or integrate (review 4, item 8).

## Revision ancestry

### The bug this fixes (exists today)

`join_revisions` (`crates/wheat-store/src/repository_integration.rs:355-447`) walks
every ancestor of every candidate (`revision_ancestors`, breadth-first over
predecessors and `combines`, linear in the change's history), takes the first of the
earliest-sorted line's ancestors that the other lines share, and does a three-way edit
merge against it.

The examples review 2 (item 1) found are fixed by the prerequisite, because a combined
revision now records every input in `combines` and is always authored: S = {}; A adds
`a`, B adds `b`; integrate makes J = {a, b} combining A and B; B is revised to B2,
removing `b`; integrating J with B2 now finds B and keeps the removal. They stay as
regression tests (review 4). What still fails today:

- Three or more inputs merged against one shared base. S = {}; M = S + `e`; X revises
  M, removing `e` (X = {}); Y revises M, adding `y` (Y = {e, y}); Z revises S, adding
  `z` (Z = {z}). The only ancestor all three share is S, which lacks `e`, so `e` is
  kept from Y and X's removal is lost. Pairwise, X and Y merge over M to {y}, and
  that with Z over S to {y, z}.
- Criss-cross histories, where the first shared ancestor found is not a best common
  ancestor (example under "Merge base").
- Decision 12's case (under "Definitions").
- Cost: `revision_ancestors` walks every ancestor of every input before anything else,
  linear in the change's history, and the ancestor pruning uses it too.

### Format (from the prerequisite)

The prerequisite (`wheat-incremental-validation.md`) adds `combines` (the other
inputs of a combined revision, at most 64) and `generation` (1 + the largest parent
generation) to revisions, makes `join_revisions` always author one combined revision
for incomparable inputs, replays combined revisions with reapplication rules, and
validates all of it. All of that has landed: the prerequisite's `join_revisions` now
always authors a combined revision (predecessor = earliest by `committed_at`,
`combines` = the rest) and `semantic_checks.rs` `revision()` checks parents and
`generation`. What is still today's is the merge rule itself (the single
nearest-shared-ancestor base above), so the J/B2 bug still reproduces. This plan uses
those fields for merge bases and the forward check. `generation` orders ancestry walks, as Git's commit-graph generation numbers
do.

Parents of a revision are its predecessor and its `combines`. Ancestors are parents,
transitively; `anc*(r)` includes `r`.

### Merge base

`merge_base(inputs) -> MergeBase` for revisions of one change:

- Find the maximal common ancestors (review 2, item 5) with Git's method: a
  max-generation priority queue seeded with the inputs, each entry carrying a bitmask
  of the inputs that reach it; an entry reached by all inputs is common and its
  ancestors are marked stale; stop when every queued entry is stale. Then drop common
  ancestors that are ancestors of other common ancestors (a second bounded walk, again
  by generation). This is exact.
- One maximal common ancestor: its edits are the base.
- Several (criss-cross, review 2, item 2): merge them recursively into a virtual base
  (their own merge base, then the three-way rule below), as Git's recursive strategy
  does. Example: bases A = {a}, B = {b} over S = {}; virtual base {a, b}; inputs
  X = {b, x} (removed `a`) and Y = {a, y} (removed `b`) merge to {x, y}.
- None: the empty base.
- Cost (review 3, item 7): in a linear or simply forked history, the revisions of
  this change between the inputs and their merge bases. Building a virtual base walks
  below the outer bases to the bases' own merge bases, so a deeply criss-crossed
  history can walk most of this change's revision history. That is still
  independent of other changes and of repository size. Virtual bases are memoized
  per command (keyed by the sorted parent set). Bounded by
  `MergeBudget { max_revision_reads, max_recursion_depth, max_edits }`; over budget
  is an error naming the change, not a wrong merge.

### Edit merge

Inputs are merged two at a time (review 3, item 2), in `committed_at` order (ties:
lowest ID); each step is a virtual revision whose parents are the two inputs, so the
next step finds its merge base through them. A single base for three or more inputs
can bring back a removal (X/Y/Z example in review 3), which pairwise merging avoids.

For two inputs `p`, `q` over base edits `E(b)`, with `R(x)` the edits removed on
`x`'s line since the base (the union of the `removed` sets of `anc*(x) \ anc*(b)`,
read during the merge-base walk):

- an edit of `E(b)` is kept when both inputs have it;
- an edit not in `E(b)` is kept when an input has it and the other input either has
  it too or did not remove it (`e ∉ R(other)`; decision 12).

Without decision 12 this is today's `join_revisions` rule with a correct base.

### `join_revisions`

- Drop inputs that are ancestors of another input, with a generation-bounded walk
  that shares the merge's budget and memo, replacing `revision_ancestors` (review 4,
  item 6). The prerequisite already follows predecessor and `combines` here, but over
  the whole history.
- One left: it is the result (`Existing`).
- Several: the prerequisite already authors one combined revision (predecessor = the
  earliest input, `combines` = the others, at most 64). This plan changes only its
  edits: the pairwise in-memory merge above, against correct merge bases, instead of
  today's rule. The virtual intermediates of the pairwise merge are never persisted.
- Virtual nodes (review 4, item 7): merge-base walks run over a node type that is
  either a stored revision or an in-memory virtual one (the pairwise intermediates and
  memoized virtual bases). A virtual node has parents (its two inputs, or the bases
  it merges), generation 1 + the largest parent generation, its edits, and the edits
  it removed (`R`). It has no `RevisionId`; it is identified within the command by its
  sorted parent set. For a virtual base, `anc*(b)` is the union of `anc*` of the bases
  it merges.
- Ordering, so the authoring client and a validating server merge identically: inputs
  sort by `(committed_at.seconds, RevisionId bytes)`; the first is the predecessor.

### Squash

`crates/wheat-store/src/repository_rewrite.rs`, `crates/wheat-client/src/squash.rs`:

- Squash never collapses a combined revision (review 3, items 3 and 10), nor a
  revision another line descends from (decision 14). It collapses only the plain
  revisions after the boundary, which is the latest of: the basis's revision of the
  change, the first remotely retained revision, the last revision with `combines` on
  the primary chain, and the latest branch point.
- Branch point: a suffix revision that a revision of this change held by another
  frontier or labeled view descends from (not counting the squashed view's own
  line). Every line that branched off survives in some frontier or label, because
  views never drop changes and a combination descends from its inputs. For each such
  revision not already on the squashed line, a generation-bounded ancestry walk stops
  once it passes below the suffix's lowest generation. Cost: the frontiers and labels,
  times the walk; `descendant_frontiers`' follow-up below applies. So every revision with `combines`
  is exactly the merge of its parents, which validation relies on, and the
  replacement's only parent is the boundary.
- A squash may still collapse a revision only this view's own line holds. Its
  replacement has the same edits, so a move from the old revision is still forward
  (edit-equal, decision 6), which is correct: nothing was lost (review 4 corrected the
  earlier claim that it becomes non-forward).

### Validation (added to the prerequisite's checks)

The prerequisite follows `combines` everywhere predecessor is followed and validates
parents, `generation` and the exact transition partition. This plan adds, for each
revision with `combines` (full, bootstrap and incremental validation):

- Its parents (predecessor and `combines`) are pairwise incomparable: none is an
  ancestor of another (review 4, item 1). Without this, a forged C with predecessor
  P = {e} and `combines` = {Q}, Q a descendant of P that removed `e`, equals its
  parents' merge yet lets a later R = {e} on Q's line look removed again.
- Its predecessor is the first of its parents in the order above.
- Its edits equal the merge of its parents (recomputed with `merge_base`), so a forged
  merge parent cannot "remove" edits.

Cost: one merge per new combined revision. Merge work (revision reads for the
merge-base walks, edits compared, virtual-base recursion) is metered by `MergeBudget`
(decision 15), not by `IncrementBudget`'s object-read meter, so an honest merge with a
distant base does not exhaust the increment's allowance. Reads the merge makes do not
pass through `Metered`. Each combined revision's check gets its own
`MergeBudget::STANDARD`, under every arrival mode (incremental, bootstrap, forced/full),
so validation's merge work grows with the combined revisions validated and no single
merge can take more than the standard budget; integration uses the same budget per
joined change, so a merge the author could make is one a validator accepts. The limits
are constants, set from the step 1 benchmark's slopes, and over budget is an error
naming the change. Memory is bounded by the same limits (the walk's queue and
the memo hold at most `max_revision_reads` entries).

## Definitions

**Forward.** A move of a label from view `O` to view `N` is forward when, for every
change `c` that `O` holds at revision `o`, `N` holds `c` at some revision `n`, and
merging `o` into `n` changes nothing: `n == o`, or `o ∈ anc*(n)`, or the edit merge
of `o` and `n` over `merge_base(o, n)` has exactly `n`'s edits.

This is the same merge integrate does, so "the label may move from O to N" means
"integrating O into N would change nothing". A descendant (including a combined
revision) is forward from each ancestor. A revision with the same edits as `o` on an
unrelated line is forward (decision 6), and so is a later revision of that line that
removed one of them (decision 12: O = {e} and R = {e} independent, N revises R to
{}; O → N is forward). `N` may hold changes `O` lacks. Subsumed edits are in their
revision's `removed` set and need no special case.

Wheat has no operation that drops a change from a view, so a view lacking a change
another view holds has not seen it yet.

**Covered.** A view `F` is covered by `N` when the move `F → N` is forward.

**Left behind** by a move `O → N`, enumerated in the repository state before the
operation (review 2, item 7): the descendant frontiers of `O`, and `O` itself when it
has none, that `N` does not cover. Coverage is checked against `N` as prepared.

## Store (`wheat-store`)

### Forward check

New `crates/wheat-store/src/label_move.rs`:

```rust
pub enum NotForward {
    Missing { change: ChangeId },
    // The merge of `o` into `n` would add edits `n` lacks, or drop edits `n` holds.
    Diverged { change: ChangeId, edits_added: usize, edits_dropped: usize },
}
impl RepositoryReader {
    pub async fn forward(&self, from: ViewId, to: ViewId, limits: &mut MergeBudget)
        -> Result<Option<NotForward>>;
}
```

- A new `RevisionMapStore::diff` over `radix::paired_diff` (as `ChangeMapStore` has
  in `change_map.rs`). `Removed` → `Missing`; `Added` → fine; `Changed(o, n)` →
  ancestry first (generation lets `o ∈ anc*(n)` stop once the walk passes `o`'s
  generation), then the merge.
- Reads: revision records only; edit sets come from the records.
- `MergeBudget` is shared by every check of one command or publish (review 2,
  item 11): revision reads, edits compared, map-diff entries, recursion depth.
- The revision-map diff is the existing `RevisionMapStore::delta`
  (`revision_map.rs:223`), which already runs over `radix::paired_diff`.

### Integrate: frontiers that lack label changes

`crates/wheat-store/src/repository_integration.rs`:

- `frontier_revisions` stops refusing `Removed`; it returns `(added, revised,
  lacked)`.
- `start_below` treats each lacked change like a revised one: candidate start = the
  view below it in the basis's composition (`view_below`); the lowest wins. Every
  change of the start is then held by every frontier, so the second
  `frontier_revisions(&start, …)` pass keeps its invariant.
- Above the start, the joined set is the union of the basis's changes and the
  frontiers'; the basis's revisions are inputs to `join_revisions` alongside the
  frontiers'.
- Lineage: an edge from the basis view to the result as well as from each joined
  frontier, so the result descends from the label's view.
- `IntegrationOutcome.reapplied` keeps its meaning. Module doc updated.

### Label move inside the mutation publication

`prepare_mutation_publication` / `finalize_mutation_publication`
(`crates/wheat-store/src/mutation_publication.rs`, used by the CLI; it hardcodes
`labels: old.labels` today) take an optional `LabelMove { name, from, to }`. Prepare
checks `labels[name] == from`, writes the new manifest object with the state, and
keeps its flush-before-CAS ordering. `RepositoryMutator::publish` gets the same
parameter. The forward and left-behind checks run in the caller between the
mutation and prepare.

### Fetch: label changes from the remote

A label's value is the whole `ViewLabel` (view, annotation, provenance). `merge_labels`
(`repository_union.rs`) returns the merged manifest plus every label whose value
differs from the local side's, with both values. Rules (review 2, item 10):

First (review 3, item 9): local and remote equal (both deleted, both created or
changed to the same value) → that value, as today.

| local vs base | remote vs base | result |
| --- | --- | --- |
| unchanged | created / moved / metadata changed | remote, then the left-behind check |
| unchanged | deleted | deleted, then the left-behind check |
| changed | unchanged | local |
| moved | moved, same metadata | `forward(local, remote)` → remote; `forward(remote, local)` → local; neither → remote, then the left-behind check |
| any other both-changed (metadata differs, one deleted, both created differently) | | refuse, as today |

The left-behind check (CLI, `native_fetch`): every view left behind by
`local value → new value` (for a deletion: every descendant frontier of the local
value, and the value itself) must be the effective view of a workspace registered
with this store (`workspace_registry.rs`, `StableWorkspaceRoots`); otherwise fetch
refuses before moving the head, naming the label and the views. A label created
remotely leaves nothing behind.

### Publish validation (server trust boundary)

The prerequisite's `validate_arrival` checks new revisions, views and labels
(retained views, `MAX_LABELS`); the merge check for combined revisions (above) goes
there, for every arrival mode.

Label authorization is separate from arrival validation (review 4, item 3), because
arrival validation also runs on fetch, where a remote label move that was forced on
the server must still arrive, and because a forced publish (`Arrival::Forced`) runs
full validation with no old envelope to compare labels against:

- `crates/wheat-server/src/commit.rs`, on every publish (forced or not, on acceptance
  and on cross-process rejoin): compare the labels of the head being replaced with
  the labels of the head being written. Each moved label: `forward(old, new)` unless
  that request's `force_labels`. No left-behind check on the server: frontiers are a
  client notion. The permission belongs to one request, not to the others joined in
  its batch.
- The server's envelope union (`union_envelopes`, used at `commit.rs:205` and `:506`)
  does not use fetch's "neither forward → remote" row (review 4, item 4). A label the
  proposal moves and the server's head moved differently refuses the publish as stale
  (decision 5), so the client pulls and integrates. `merge_labels` takes the policy as
  a parameter: `Fetch` or `Publish`.
- One `MergeBudget` per publish covers the label checks.

## CLI (`wheat-cli`)

### Authoring on label-held changes (decision 9)

- `wheat add`, `change select`, `change squash` and `stack squash` accept any change in the
  workspace's effective view (the "is not active in this workspace" refusals in
  `crates/wheat-client/src/authoring.rs:138,410` and `squash.rs:245`, fed by
  `active_changes::inspect`). "Active" stays the name for changes revised since the
  basis, used in listings. Capture, conflict resolution, status and
  `create_on_frontier` already work over the effective view (review 2, item 9).
- Revising a label-held change makes the result a frontier that revises a basis
  change; integrate handles that (5a4fdb6).
- Squash of a label-held change collapses only revisions after its boundary
  (above); with none, it does nothing and says so.

### `wheat label move <name> [--view <view>] [--force] [--expected-head …]`

- Default `--view`: the workspace's effective view.
- Refuses when not forward (naming the change) or when it would leave frontiers
  behind (listing them); `--force` skips both.
- CAS envelope mutation without full validation: it checks the target view is
  retained in the current state, then the checks above.
- Moving the workspace's active label to the workspace's effective view rebinds this
  workspace (decision 16): `basis` and `observed_label_binding` become the new view
  and `selected_frontier` becomes `None`. Materialization is unchanged. With `--view`
  elsewhere, the workspace keeps its basis, observed binding and selection, and shows
  drift until `refresh` or `integrate`.
- Crash safety (review 4, item 9): the rebind is a recoverable intent, like
  `selection_recovery.rs`: the complete old and new workspace binding (basis,
  observed binding, selected frontier) is staged before the CAS and finished after
  it, with the workspace registry pins reconciled. A crash on either side of the CAS
  leaves a workspace the next command completes or rolls back, never a half rebind.
  Tests cover the crash points around the CAS and the metadata write.

### `integrate`, `pull` and `refresh`

- Basis: the active label's current view.
- Frontiers: the label's descendant frontiers, plus the workspace's effective view
  when the label's view does not cover it (a fetch moved the label underneath it).
- One frontier and the basis: only when that frontier covers the label's view does
  the command just switch to it; otherwise it integrates the frontier with the basis.
- After the join, the active label moves to the result in the same CAS, unless
  `--keep-label`. If the move would leave frontiers behind (only with an explicit
  `--frontier` subset), the command refuses before writing unless `--keep-label` or
  `--force`.
- `refresh` (decision 11) does what `integrate` does when the workspace's view is not
  covered by the label's view, and otherwise rebinds as today; it moves the label
  like integrate, honouring `--keep-label`.
- JSON gains `"label_moved": {from, to}` or `null`.

### A workspace the label moved away from (review 2, item 8)

Its `basis` and `observed_label_binding` still name the old view and are consistent
with each other; only the label changed. Authoring (`add`) keeps working against the
old basis (`repository_mutation.rs:719-733`, `open`, checks the selection against the
workspace's basis, not the label); the result is a frontier of the old view, which
the next integrate joins. Only `refresh`, `integrate`, `pull` and `switch` re-read
the label.

Two things change for this to hold (review 3, item 5):

- `materialize` stops re-resolving the label (`materialize_active_workspace`,
  `crates/wheat-cli/src/main.rs:1520-1560`, calls `frontier_service::resolve`). It materializes the
  workspace's pinned basis and selected frontier, checking only that they are
  retained in the current head.
- The materialization journal's `authenticate_target`
  (`crates/wheat-client/src/materialization.rs:1135-1166`) stops requiring the label in the journal's target
  head to name the workspace's binding. Capture settlement advances the journal's
  pinned head to the new publication head (`capture_settlement.rs:231-240`), whose
  label may already name another view, so that check would fail. It checks instead
  that `basis == observed_label_binding` within the journal and that the views it
  names are retained in the target head. The journal still pins a head, so recovery
  stays exact.

### `publish`

- Before connecting: unless `--keep-label`, the target is the workspace's effective
  view. Already there → nothing to move. Otherwise check forward and left-behind; on
  failure refuse (nothing sent), pointing at `--keep-label` and `--force-label`.
- The move is a local CAS made before the remote publish, rebinding the workspace as
  `label move` does; the published envelope carries it. If the remote refuses
  (stale, validation), the local move stays and the output says so; the next pull
  reconciles it through the fetch rule. The published changes become label-held,
  which decision 9 makes harmless for further editing.
- `--force` keeps meaning `allow_non_monotonic`; `--force-label` sends
  `force_labels`.

### Removing full validation from the paths these commands take

Done by the prerequisite (its step 9): `frontier_service`, `label_service` and
`materialization::authenticate_target` read the envelope and check the views they use
with `RepositoryReader::retained_view`; views are validated when they arrive (publish
on the server, clone and fetch on the client). `wheat validate` keeps full validation.

## Protocol

`PublishRequest` (`crates/wheat-core/schemas/wheat-protocol.bs.json5`) gains
`force_labels: bool`; regenerate (`just protocol-gen-schema`), update
`crates/wheat-protocol/tests/{round_trip,forged_lengths}.rs`, `remote::publish`, and
`crates/wheat-server/src/publish.rs` passes it to validation. The flag is part of the
publish request's identity (review 4, item 10): it joins the request-digest input in
`wheat-objects.bs.json5` and `publish_request_digest` (`receipt.rs:56`), so a retry
key reused with a different `force_labels` is a different request.

## Git bridge (`wheat-git`)

- A moved label exports as a ref update. A forward Wheat move that revised a change
  is not a Git fast-forward, so pushing it hits `UnknownRemoteTip`
  (`bridge.rs:115-130`) unless `--force-with-lease`, as a revised change does today.
  Documented, no change.
- A moved annotated label makes a different tag object; the bridge refuses with
  `TagExists` (`bridge.rs:220-229`). Allow replacing the remote tag with
  `--force-with-lease`; keep the refusal otherwise and say so in the error.
- Squash's guard on annotated labels (`repository_rewrite.rs:352-362`) is about
  rewriting the history a tag names; unchanged.

## Docs

- `docs/design/wheat-local-cli.md`: label move; integrate/pull/refresh/publish moving
  the label; authoring on label-held changes; fetch's label rule; workspaces the
  label moved away from; the "Defer — label advancement and frontier consolidation"
  item done; status line.
- `docs/design/server-native-source-control-store.md`: same deferred item and the
  paragraph near line 730; revision ancestry (`combines`, `generation`).
- `docs/design/wheat-native-protocol.md`: `force_labels`; the revision and label
  checks on publish.

## Tests

Store (`crates/wheat-store/tests/`):

- Regressions (fixed by the prerequisite, kept): J/B2 keeps B2's removal;
  A = {a,b}, B = {b} join followed by B2 keeps the removal.
- Failing today: the S/M/X/Y/Z three-input case; criss-cross; decision 12.
- `merge_base`: linear; two inputs; three inputs; criss-cross gives the recursive
  virtual base (X/Y example); a redundant common ancestor dropped; no common
  ancestor.
- Edit merge: S/M/X/Y/Z keeps X's removal, in every input order; decision 12's O/R/N
  case; equal `committed_at.seconds` orders by revision id.
- Integration read counts: a join with a dominated input and a two-way divergence
  after 10 vs 100 revisions of the change reads the same (the whole operation, not
  just `merge_base`); more than `MAX_COMBINES` + 1 incomparable inputs is refused
  before reading their history.
- Validation (full, bootstrap and incremental): refused are a combined revision whose
  edits differ from its parents' merge; one with a parent that is an ancestor of
  another (the forged C above); one whose predecessor is not its first parent.
- Squash stops at the last combined revision, the basis's revision, the first
  remotely retained one and the latest branch point, whichever is latest; the
  removal-evidence case (B, M = {e}, N = {}, sibling O = {e} off M in another frontier)
  merges the same before and after the squash (the client's `squash.rs` picks only one
  of the last two today); the store's `validate_boundary` refuses a boundary with a
  combined revision after it; squashing a change whose latest revision is combined
  does nothing and says so.
- `forward`: equal; descendant; descendant that removed an edit; sibling lacking an
  addition; sibling keeping a removed edit; same edits on an unrelated line
  (forward); combined from each input; missing change; extra changes.
- Read counts: `merge_base`/`forward` with a one-revision divergence after 10 vs 100
  revisions of the change, and after 16 vs 160 unrelated changes: same reads.
- Integrate with a frontier lacking basis changes: canonical order holds; both sides'
  changes; a divergent revision combined; the basis → result lineage edge.
- Publish validation: a backward label move refused without `force_labels`; over
  the merge budget refused.
- `merge_labels`: every row of the fetch table.

CLI (`crates/wheat-cli/tests/`):

- `label move`: forward, refused backward, `--force`, refused when leaving a frontier
  behind, workspace rebind to its effective view; `--view` elsewhere leaves the
  workspace's binding and selection as they were (selected and unselected).
- integrate/pull/refresh move the label; `--keep-label`; explicit `--frontier` subset
  refuses the move; one-frontier shortcut only when covered.
- publish moves the label and the remote sees it; refused when not forward or leaving
  a frontier behind (nothing sent); a refused remote publish keeps the local move.
- `wheat add` on a label-held change after publish; `stack squash` on one.
- A workspace the label moved away from: `status` shows drift, `materialize`,
  `restore` and `add` still work, `refresh` integrates.
- The race: Alice and Bob on `main`; Bob pulls and integrates (label → C); Alice
  publishes (remote → D); Bob's publish refused; Bob pulls: fetch takes D, integrate
  joins D and C, label → result; Bob publishes; Alice pulls and has both.
- Fetch refusing a label change or deletion that would hide an unselected frontier.
- Fetch accepting a remote label move the server took with `force_labels`.

Server (`crates/wheat-server/tests/`): backward move rejected, accepted with
`force_labels`; forward accepted; forged combined revision rejected; the four
combinations of `force` and `force_labels`; a label moved differently on the server
refuses the publish as stale, on acceptance and on cross-process rejoin; a retry key
reused with a different `force_labels` refused.

Benchmark (Rule 20), `crates/wheat-bench` case `label_move`: `wheat publish` with a
move, `wheat pull` integrating one change, `wheat label move`, `wheat refresh` — at
50 and 500 changes of history and at 10 and 100 revisions of the moved change, one
change differing. Contract: objects read do not grow with history (≤ 1.25× from
small to large); wall time recorded. A second shape: a change with repeated
criss-cross merges (10 vs 100 rounds), recording `merge_base` reads, which are
expected to grow with the rounds (the honest bound above), to see the slope. Smoke
size for `just test`; release run; results under `bench/results/`.

## Follow-ups (TODO.md)

- `descendant_frontiers` is linear in the views since the label's view; label moves
  keep that distance short in normal use, but publish and fetch now run it.
- `active_changes` collects the basis's whole revision map per command
  (`collect_revisions`), linear in changes.
- Combined revisions still carry no subsumption or resolution evidence.

## Order of work

0. The prerequisite project (decision 13), with its own plan.
1. Merge bases over the DAG; pairwise in-memory edit merge in `join_revisions`; the
   merge check in validation; squash's boundary. Regression tests first.
   **Done 2026-10-22**: `crates/wheat-store/src/revision_merge.rs` (`RevisionMerge`,
   `MergeBudget::STANDARD` = 4,096 revision reads, recursion depth 16, 2^20 edits
   compared; `check_combination` in full, bootstrap and incremental validation);
   `RepositoryReader::squash_boundary` and `SquashStops` in `repository_rewrite.rs`,
   called by the client's squash with every authority root's revision as held;
   `validate_boundary` refuses collapsing a combined revision. Bench case
   `revision_merge`: joining after a line of 10, 100 and 1,000 revisions reads 51, 51
   and 67 objects, validating it 93, 94 and 118 (log growth of the collections, 0.016
   and 0.025 reads per line revision; contract 0.1). Criss-cross rounds 4 and 12: 88
   and 180 reads joining, 193 and 436 validating. Each criss-cross round nests one
   virtual base, so more than 16 rounds of distinct combinations of one change exceed
   the recursion depth and integration refuses, naming the change.
2. `forward` + tests + read counts.
3. Integrate with lacking frontiers and the basis lineage edge.
4. Materialize from the pinned basis; the journal's binding check. (Full validation
   already left the read paths in the prerequisite.)
5. Label move in mutation publication; `label move`; workspace rebind.
6. Authoring on label-held changes.
7. integrate/pull/refresh move the label; `--keep-label`; one-frontier rule.
8. Fetch label rules (store + CLI).
9. Protocol field, server label checks and limits, publish moving the label.
10. Git bridge tag replacement.
11. Race test, docs, benchmark, TODO bookkeeping.
