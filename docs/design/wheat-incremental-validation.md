# Wheat incremental validation at trust boundaries — plan

Status: proposed (2026-10-15), revised after Codex reviews 1 and 2; for review
before implementation. Prerequisite of `wheat-label-moves.md` (its decision 13).
Owner: bart

## Why

What enters a store from outside is barely checked, and local commands make up for it
by validating all of history:

- **Server publish** (`commit`, `crates/wheat-server/src/publish.rs:312-329`) runs
  `validate_transition_with_options` (`crates/wheat-store/src/repository_transition.rs`).
  It checks that retained sets only grow, that new objects hash and decode, that new
  change-map entries and new views' revisions are retained, and that new content
  exists. It checks no semantics of new revisions, applications or views, and never
  reads the proposed label manifest. A client can publish a view that copies another
  view's revision map over an empty application map and an empty tree, a revision
  whose `removed` set is made up, or a label naming a view the state doesn't retain.
- **Client fetch and clone** (`native_fetch`, `native_clone`,
  `crates/wheat-cli/src/main.rs:2651-2984`; `remote.rs:516-586`) check object hashes
  and decodability, then union or adopt the remote state. No semantic check at all.
- **Local commands** call `validate_envelope` (full validation of all retained
  history) in `frontier_service::list` / `resolve`, `label_service`,
  `materialization::authenticate_target`, Git push and others, so a forged view is
  caught before materialization, at a cost that grows with history: about 30 s for
  `wheat labels` on the Gothab import (TODO.md).
- **Publish validation scans history.** Its lineage step builds both whole
  adjacency maps (`view_lineage.rs:422-447`), it walks every new view's whole
  revision map (`repository_transition.rs:179-203`), and content manifests are flat
  snapshots (about 3.3 MB per view at 100,000 files), so any content check reads
  whole manifests.

The project rule is to validate at trust boundaries (network, disk, other processes)
and not between Wheat's own crates. This plan moves the semantic checks to the
boundary, makes them proportional to what arrived, and then lets local commands
trust their store.

## Bart's decisions (2026-10-15)

1. Canonical order is validated at the boundary, using a stored position and a small
   max-ID index per application (format change).
2. Content manifests become radix structures in this project (format change), so
   content checks are proportional to what changed.

## Scope

In: the format changes above; one incremental validator used by server publish,
client fetch and clone (and the joined-state fetch after a publish); label manifest
checks;
removing full validation from local read paths; tightening full validation where the
two disagree; benchmarks.

Out: snapshot import (already fully validated); Git import (local code, trusted).

## Invariants the store keeps

Written down here because the validator proves them per increment and local commands
rely on them:

- **I1 Composition.** The base view has empty maps, no conflicts, no tips, and a
  required content map equal to its tree's content. Every other view `V` is `T + A`:
  one application `A` of change `k`'s revision on a parent view `T`, where `V`'s
  revision and application maps are `T`'s plus key `k`, `V.tree` is `A`'s result
  tree, `V`'s conflicts are `T`'s plus `A`'s conflict if any, `V`'s required content
  is `T`'s adjusted by the tree change, and `V`'s tips are `T`'s without `k`'s
  dependencies and with `k`. The revision is `k`'s and its dependencies are in `T`.
  So a view's tips are its changes that none of its other changes depend on: what a
  new change on top of it depends on.
- **I2 Canonical order.** In I1, `k` is last in the canonical order (topological by
  dependencies, ties by lowest ChangeId) of `T`'s changes plus `k`. Equivalently:
  every change of `T` that comes after `k`'s last dependency in `T`'s canonical order
  has a lower ID than `k`. (Proof sketch: nothing in `T` depends on `k`, so Kahn's
  algorithm runs as for `T` until `k`'s dependencies are placed; from then on `k` is
  available and is picked as soon as it is the lowest available ID, so it is picked
  last exactly when every later pick is lower.)
- **I3 Lineage.** Every view except the base has an incoming edge, and has a
  composition parent (I1) among its lineage parents. Every edge `P → C` has
  `weight(C) > weight(P)` (see "View weight"), so the lineage is a DAG edge by edge:
  no union of valid states and no forced history can make a cycle. Each
  application has one result view per target, because radix collections have one
  canonical encoding (I9).
- **I9 Canonical collections.** Every radix node (maps, sets, content maps) has the
  layout the builder produces for its entries (`collection_builders.rs`), so equal
  logical contents have equal IDs. `radix.rs:79` used to accept an inline entry or a
  terminal child for the same entry, so two encodings of one view could coexist.
- **I4 Changes.** The change map is a bijection between logical ChangeIds and
  retained change objects.
- **I5 Revisions.** Each revision's transition partitions its edits against its
  predecessor exactly (`predecessor.edits == preserved ∪ removed`), its predecessor is
  the same change's, and its edits replay cleanly on `authored_against`'s tree to
  `resulting_tree`.
- **I6 Applications.** Each application's evidence equals a replay of its revision's
  edits on its target's tree.
- **I7 Content.** Every content ID referenced by a retained tree, edit or conflict is
  in the state's aggregate content set and present in the content store; each view's
  required content is exactly its tree's content.
- **I8 Labels.** Every label names a retained view; at most `MAX_LABELS` labels.

## Format changes

### View size, last change and weight

`ViewRecord` (edited in place) gains `size: u64` (number of changes), `last:
Option<ChangeId>` (the last change in its canonical order) and `weight: u64` (below).
All three are determined by the view's maps, so content addressing still gives one ID
per logical view. For the base: 0, none, 0. For `V = T + A` of change `k`:
`size = T.size + 1`, `last = k`, `weight = T.weight + 1 + generation(revision)`.

**View weight.** `ChangeRevision` gains `combines: BTreeSet<RevisionId>` (the
inputs of a combined revision other than its predecessor, at most 64; empty
otherwise) and `generation: u64` = 1 + the largest generation among its parents
(predecessor and `combines`), 0 with none. `combines` is needed here, not only in the
label-move plan: today a combined revision's predecessor is the earliest input, so
its generation could be lower than a later input's, and the integrate edge from the
frontier holding that input would not increase the weight. This project also makes
`join_revisions` always author a combined revision for two or more incomparable
inputs (review 3, item 1): today it reuses an input whose edits equal the
combination, which gives an edge of equal or lower weight from the frontier holding
the other input. One combined revision is persisted per join of a change: its
predecessor is the earliest input, `combines` holds the other inputs (at most
`MAX_COMBINES` = 64), and its edits are the merge (round 4: persisting pairwise
intermediates would cost quadratic storage and refuse merges whose final result is
fine). A join with more than 65 incomparable revisions of one change refuses, naming
the change and saying to integrate fewer frontiers at a time (`--frontier`). The
label-move plan later changes which merge base the edits are merged against
(computing it pairwise, in memory); this project keeps today's edit rule.

A combined revision's edits may already be satisfied where it lands (identical
inputs whose change another change already made). So a revision with `combines` is
replayed, both when authored and when validated, with the rules a reapplied revision
follows: `AlreadySatisfied` is allowed, a conflict is not (`RevisionsDoNotCombine`
when authoring). Full validation's replay check (`repository_verify.rs:1172-1178`,
which accepts only `Applied`) takes the same rule for revisions with `combines`.
`author_combination` stops using a persisted preliminary application with a dummy
revision ID (`repository_integration.rs:434-440`): it replays without persisting,
then writes the revision, then applies it normally.

Everything that follows `predecessor` follows `combines` too (review 3, item 3):
`object_graph.rs:213-219` (snapshots, transfer), `structural_delta.rs` pairing,
rewrite retention (`repository_rewrite.rs:507-516`), reconstruction
(`wheat-core/src/repository/reconstruction.rs:64-78`), encoding and hydration.
Arithmetic on `generation`, `size` and `weight` is checked; an overflow is a
validation failure. (Codex's bound: with validated ancestry, weight is at most the
number of retained revisions, so honest histories cannot overflow.) A view's weight is the sum over
its changes of `1 + generation(revision)`. Every honest edge increases it:
composition adds a change; a revise, resolve or continuation replaces a revision by a
descendant (higher generation); integration's result holds every change of each
joined frontier at the same revision or a descendant, plus possibly more. Squash
writes a new history whose own edges follow the same rules. So the rule "every edge
`P → C` has `weight(C) > weight(P)`" holds for everything Wheat writes and makes
cycles impossible, checked one edge at a time. This replaces review 1's "edges only
into new views" rule, which broke replication (review 2, items 3-5): an edge into a
view the receiver already has is fine when it increases the weight.

### Application position and order index (decision 1)

`ApplicationRecord` (schema `ApplicationV1`, edited in place) gains:

- `position: u64` — `T.size`, the index of `k` in the result view's canonical order.
- `previous: Option<ApplicationId>` — `T`'s producing application,
  `T.applications[T.last]` (none when `T` is the base). Determined by `T`, so it is
  the same in every history that reaches `T` (review 2, item 1).
- `skip: Vec<ApplicationId>` — `skip[j]` is the application at position
  `position - 2^j`, for each `j` with `2^j ≤ position` (empty at position 0).
  `skip[0] = previous`, `skip[j] = skip[j-1].skip[j-1]`.
- `window_max: Vec<ChangeId>` — `window_max[j]` is the highest ChangeId among
  positions `[max(0, position - 2^j + 1), position]`, for each `j` with
  `2^j ≤ position + 1`. `window_max[0] = k`;
  `window_max[j] = max(window_max[j-1], skip[j-1].window_max[j-1])` for `j ≥ 1`
  (`skip[j-1]` exists: `2^j ≤ position + 1` implies `2^(j-1) ≤ position`).

About 20 entries each at a million changes. `apply_step` computes them from
`previous` and the `skip` chain in O(log n) reads.

**Query** `max_id(T, a)`: the highest ChangeId over `T`'s positions `[a, T.size)`.
Start at `T`'s producing application `x` (position `T.size - 1`) and remaining length
`L = T.size - a`; while `L > 0`, take the largest `j` with `2^j ≤ L` and `2^j ≤
x.position + 1`, fold `x.window_max[j]`, set `L -= 2^j`, and move to
`x.skip[j]` (which exists whenever `L > 0` remains). O(log n) reads.

**I2 check** for `A` onto `T`: `p` = 1 + the largest `position` among `k`'s
dependencies' applications in `T` (one map lookup each; 0 with none);
`max_id(T, p) < k` (or `p == T.size`).

**Validation of the fields** for a new application: `position == T.size`; `previous
== T.applications[T.last]`; each `skip[j]` and `window_max[j]` recomputed from the
recurrence. Tests cover positions 0, 1, 2, 3, 4, 7, 8 and 1,000.

### Content maps (decision 2)

`ViewRecord.required_content` and the state's aggregate content set become radix maps
`ContentId → count` (the number of tree entries naming that blob), built on the same
radix machinery as the retained sets (`sets.rs`). Counts fix the "is this blob still
used elsewhere in the tree" question that a set cannot answer. The aggregate becomes a
radix set of ContentIds (no counts: membership only). `ContentSetSnapshotV1` /
`ContentSetDeltaV1` and `ContentSetStore`'s chain logic go away; its users
(`commit_content.rs`, `commit_writer.rs`, `repository_mutation.rs`,
`repository_union.rs`, `repository_rewrite.rs`, `repository_hydration.rs`,
`repository_verify.rs`, `remote.rs`, the server, `wheat-git` import's accumulator)
move to the new store. Updating a view's map costs the tree diff, not a manifest
rewrite (`commit_content.rs:54-69` scans the whole tree today). This also closes the
TODO.md item "Content manifests are full snapshots".

Storage, measured 2026-10-15 by replaying Gothab's 1,905 first-parent commits into a
32-byte-key radix set (loose bytes, before repack):

| encoding | total | per view (p50 / p90 / p99) |
| --- | --- | --- |
| flat snapshots (today) | 218.5 MiB | ~117 KiB avg |
| radix, per-edit updater (before 7551122) | 377.1 MiB | 72 / 446 / 1,996 KiB |
| radix, batched updater | 32.1 MiB + ~1.9 MiB counts | 11 / 31 / 94 KiB |

651c01b moved away from radix content sets because of the 348 MiB they cost. That was
the per-edit updater writing a new root for every key of a batch, and 7551122 fixed
it.

Paths that need explicit changes beyond the store users (review 2, item 8):

- `wheat-core`: view construction and hydration hold `required_content` as a
  `BTreeSet` (`model.rs:548`, `repository_hydration.rs:88`) and canonical
  serialization derives view IDs from it; they move to the counted map, so IDs come
  from the radix root.
- Structural transfer (`structural_delta.rs:846`) accepts snapshot/delta content
  objects and follows their chain; it follows radix children instead, with pairing
  by key prefix like the other radix maps.
- Aggregate negotiation (publish's content wants, `fetch_content`) and repack keep
  set semantics: they use the aggregate set, never the counts.
- Git import's accumulator (`content_accumulator.rs:85-123`) reads each view's
  manifest once. With counted maps it diffs each view's map against its composition
  parent's (review 3, item 6), seeding the base's contents once, so it stays
  proportional to what each commit changed. Diffing consecutive accumulator entries
  would not: they are ordered by commit or root ID, not by history. A work-count
  benchmark imports a branching history with shuffled commit IDs.

## The validator

`RepositoryReader::validate_increment(old, new, mode, budget) -> Result<IncrementReport>`
in `crates/wheat-store/src/increment_validation.rs`, replacing
`validate_transition_with_options`. `old` (state and labels) is trusted; see "What is
trusted". Modes: `Extend` (ordinary), `Bootstrap` (clone: no old state), `Forced`
(`allow_non_monotonic`).

### Finding what is new, without trusting what merely exists

New entities come from the four retained-set deltas (radix `paired_diff`,
proportional to the delta). Everything else is reached from a new entity by a diff
against a trusted counterpart, so shared subtrees are skipped by ID because they are
part of an already validated object, never because they happen to be in the store:

- a new view's maps, tree, conflicts and required content: diffed against its
  composition parent `T` (old or new; a new `T` is validated first, in topological
  order of the new views);
- a new revision's resulting tree: diffed against `authored_against`'s tree, with the
  edits replayed;
- a new application's result tree: diffed against its target's tree, with the edits
  replayed; its conflict decoded and checked;
- edit records: decoded when replayed;
- the lineage map, change map and content maps: `paired_diff` against `old`'s.

Every radix node is checked to have the canonical layout for its entries (I9), a
local check per node. It sits in the position-aware read (`radix::validate_node_at`,
used by every radix read and by `object_graph`), so both validators get it without
extra work. Each radix node on a new path is read anyway. A
value on a non-root node is refused, and the edge rules are checked when the node is
decoded. Review 3, item 5 found that the updater wrote a temporary terminal child
during deletion, then read it back and inlined it. Bart chose to fix the updater
instead of moving the check out of the read: `update_node` now hands a lone remaining
entry to its parent (`Updated::Leaf`) and writes nothing non-canonical. Parity tests
(`collection_builder_parity.rs`): for every radix family
(revision and application maps, retained sets, lineage map and child sets, change
map, conflict sets, content maps), random insert/delete sequences through the
updater give the same root as building the final entries from scratch.

### Steps

1. **Monotonicity** (`Extend`): retained sets, change map, lineage edges and
   aggregate content only grow; base view unchanged.
2. **Changes (I4):** each new change object's logical ID has exactly one change-map
   entry naming it; no new entry names an ID an existing entry already has; changed
   entries only in `Forced`.
3. **Revisions (I5)**, per new revision: change, dependencies, `authored_against`,
   predecessor and every `combines` entry retained by `new`; every parent of the same
   change; `combines` empty for ordinary revisions and 1 to 64 entries for combined
   ones, never the predecessor or the revision itself;
   `generation` = 1 + the largest parent generation (checked arithmetic); the exact
   partition against the predecessor; subsumptions and resolutions as full validation's step 10
   (`repository_verify.rs:1112-1139`); replay to `resulting_tree`.
4. **Applications (I6)**, per new application: revision and target retained; replay,
   evidence, result tree, conflict (`verify_conflict`); the position and order-index
   fields validated from `previous`. Every new application must be the composing
   application of some view retained by `new` (review 3, item 2): step 5 marks each
   application it uses, and an unmarked new application is refused as orphaned.
5. **Views (I1, I2)**, per new view `V` in topological order of the new lineage
   edges:
   - Parent discovery: `k = V.last` and `A = V.applications[k]` name the composition
     parent directly: `T = A.target_view`. There must be a lineage edge `T → V`.
     Then diff `V`'s maps against `T`'s (canonical encodings, I9, so the diff walks
     only the path to `k`): exactly key `k` added, nothing else different.
   - `A.revision_id == V.revisions[k]`, the revision's `change_id == k`, each of its
     dependencies is a key of `T`'s revision map; `V.tree` is `A`'s result tree.
   - `V.size`, `V.last`, `V.weight` as defined from `T` and `A`.
   - Conflicts: `V`'s set is `T`'s plus `A`'s conflict, or equal (set diff).
   - Required content: the tree diff `T.tree → V.tree` gives count changes per
     ContentId; `V.required_content` must equal `T`'s plus those (map diff, same
     size as the tree diff); every ContentId whose count goes from 0 to positive is
     in the aggregate (I7).
   - Tips: the set diff `T.tips → V.tips` is exactly `k` added and each of `k`'s
     dependencies that is in `T.tips` removed (contains lookups, and a diff that
     stops past that many entries).
   - I2 via the order index.
6. **Lineage (I3):** from the lineage map diff, every added or changed lineage-map
   key is retained by `new` and its child set is non-empty (review 3, item 4: an
   empty child set under an unretained key has no edges to check; the edge builder
   never writes one, `collection_builders.rs:337-354`); every new edge's endpoints
   are retained by `new`, and every new edge `P → C` has `weight(C) > weight(P)` (two
   view reads). That makes the lineage acyclic without any graph walk, whatever
   other valid states it is later unioned with. Every new view has its composition
   edge (step 5). One result view per application and target follows from I9.
7. **Content (I7):** new aggregate members exist in the content store (as today);
   every ContentId a new tree, edit or conflict names is in `new`'s aggregate (typed
   membership, not "the blob exists on disk").
8. **Labels (I8):** every created or moved label names a view retained by `new`
   (every label, in `Forced`); at most `MAX_LABELS` labels, a cap that moves to
   `wheat-core` and is enforced when a manifest is decoded.

### Bootstrap (clone)

No old state. The base view is checked against I1's base rule; every other retained
object is new and goes through steps 2-8, so the cost is the repository's size, as a
clone already is. A base view other than the one in the remote state does not arise.

### Forced (`allow_non_monotonic`)

Removals can leave unchanged objects pointing at something gone (a view still
selecting a removed revision, an edge's last incoming edge removed), which the
incremental steps don't see. A forced publish or forced fetch runs full validation
(`validate_complete`) on the new state plus step 8 for every label (labels are
`validate_envelope`'s, not `validate_complete`'s, review 2, item 6). Full validation
proves the same invariants as the incremental steps (see "Full validation
changes"), so a forced state is as trustworthy as an incremental one. Forced
operations are rare and explicitly destructive; their cost is documented.

### Budget

`IncrementBudget` counts the work actually done: objects decoded, bytes decoded,
map-diff entries, edits replayed, content bytes read for replay. The caller sets it
from the size of the logical increment (new entities and the edits they carry), not
from bytes transferred, since a cached increment transfers nothing and an
application can replay a large old revision. Over budget → refused with a reason,
never a partial validation.

## What is trusted

- **The old side of a publish** is the server's current head: validated when it was
  published.
- **The old side of a fetch** is the last fetched envelope, recorded in
  `remote.json`. It must only ever be set to an envelope this client validated:
  - after a fetch, to the remote envelope that passed `validate_increment`;
  - after a publish, to the envelope this client offered, or to the joined envelope
    the server returns. Since 79bbb99 the server joins a publish that is behind its
    head into the head (`commit`, `crates/wheat-server/src/publish.rs:256-409`), and
    the client fetches the joined state over the same session. That fetch is an
    increment like any other: `validate_increment(offered_state, joined_state)` (the
    offered state is the client's own, so trusted) must pass before the client moves
    its head or records the joined envelope.
  - after a clone, to the bootstrapped envelope.
  - The unchanged-head shortcut in fetch (`main.rs:2861-2868`) is safe once this
    holds.
- **The union** of two validated states keeps every invariant: I1, I2 and I5-I9 are
  per object or per edge (the weight rule makes acyclicity per edge, so two sides'
  edges cannot combine into a cycle, review 1, item 1), and I4's bijection is
  enforced by `union_states` (it refuses a ChangeId with two objects). Unions made
  from a forced history are covered too, since forced states pass the same
  invariants (review 2, item 4).
- **Local mutations** stay unvalidated (`RepositoryMutator::publish`): our own code,
  covered by tests and by `wheat validate`.

## Integrate and existing views

Integrate keeps adding its edges when the result already exists (review 2, item 5):
with the weight rule an edge into an existing view is valid whenever it increases the
weight, which every integrate edge does. Frontiers are retired and the label-move
plan's basis → result edge is allowed.

## Full validation changes

To keep full and incremental validation proving the same invariants:

- transition partition: equality instead of subsets (`repository_verify.rs:1101-1110`);
- every view other than the base satisfies I1 with its composition parent named by
  `last` (review 2, item 6), and the base satisfies the base rule;
- the "exactly one child per application" rule identifies composition edges by maps
  instead of by tree (`repository_verify.rs:890-913`); rewrite's `produced_view`
  (`repository_rewrite.rs:434-464`) uses the same identification;
- the weight rule on every edge; `size`, `last`, `weight` and `generation`;
- I2 via the order index, and the order-index fields;
- I9 on every radix node;
- the new content maps.

## Local commands trust the store

Once the boundary validates, these drop `validate_envelope` / `validate_complete` and
check only what they use (envelope, state and label manifest decode; the views they
name are retained):

- `frontier_service::list` and `::resolve` (`frontier_service.rs:46,130`): init,
  materialize, switch, refresh, pull, integrate, clone, frontier list.
- `label_service::load` and `::mutate` (`label_service.rs:182,220`), and the
  publisher calls it makes (`prepare_envelope`, `publish_envelope`).
- `materialization::authenticate_target` (`materialization.rs:1128`): it keeps its
  binding, selection and tree checks, only the history-wide validation goes.
- Git `push` (`main.rs:2261,2338`).

Unchanged: `wheat validate`; `prepare_squash` and other rewrites; `plan_repack`
(relies on it, `packed/store.rs:360`); snapshot import/export; general imports.

A store edited by hand or written by an older build is not protected by this;
`wheat validate` is the tool for that, and the docs say so.

## Tests

`crates/wheat-store/tests/increment_validation.rs`, each forging a state from a valid
one and expecting refusal, and the valid one passing:

- revision: wrong change for predecessor; `removed` not in the predecessor;
  predecessor edit neither preserved nor removed; replay mismatch; content not in the
  aggregate.
- application: evidence not matching replay; wrong result tree; conflict mismatch;
  wrong `position`, `previous` (including review 2's `[1, 2, 4]` + 3 case with the
  application of 2 as `previous`), `skip` or `window_max`; index at positions 0, 1,
  2, 3, 4, 7, 8, 1,000.
- view: the copied-revision-map forgery; two keys differing; application targeting
  another view; revision of another change under key `k`; dependency missing from
  `T`; tree not the result; conflicts or required content wrong (including a blob
  used at two paths with one removed); wrong `size`, `last` or `weight`; out of
  canonical order (B → A with A < B refused; "k = 1 depends on z = 3 after y = 2"
  accepted); no composition edge.
- radix: a non-canonical node (inline entry re-encoded as a terminal child) refused;
  an application with a second, re-encoded result view refused.
- lineage: an edge that doesn't increase the weight (including a shortcut and a
  back edge); an edge into an existing view that increases it accepted (the
  replication case from review 2: A, B, C integrated as R remotely and via X
  locally); an old edge dropped.
- revisions: wrong `generation`; `combines` naming another change, the
  predecessor, itself, or with more than 64 entries; generation overflow; a combined
  revision whose edit is already satisfied validates, an ordinary one doesn't.
- applications: an orphaned new application (no view composed by it) refused.
- lineage map: an empty child set, or a key naming an unretained view, refused.
- integrate: two sibling revisions with identical edits get a combined revision
  (no reuse) and the edges from both frontiers increase the weight; 65 divergent
  revisions of one change combine, 66 refuse naming the change.
- changes: a new change object without a map entry; a second object for an existing
  ChangeId; a key not matching the change.
- labels: created or moved label on an unretained view; forced rewrite dropping a
  labelled view (refused by the full-validation fallback); over the cap.
- union: two increments that would form a lineage cycle together cannot both be
  valid (one of the edges fails the weight rule); a forced history unioned with an
  ordinary one passes full validation.
- format round trips: hydration and snapshots keep view IDs with counted content
  maps and the new view fields; shared subtrees at several paths count correctly.
- budget exceeded; a cached increment (nothing transferred) still validated within a
  budget sized by its logical increment.
- Equivalence: across built histories with branching, conflicts, integrate (including
  a result that already exists), resolve, squash, forced rewrites and unions, every
  step passes `validate_increment` (or the forced fallback) and the result passes
  `validate_complete`; each forgery above that full validation would refuse is
  refused incrementally too.
- Read and byte counts: a one-change publish after 50 vs 500 changes, and with
  1,000 vs 100,000 files spread over directories, decodes the same number of objects
  and about the same bytes (≤ 1.25×). Bytes still grow with the size of the
  directory records the change touches (review 2, item 9): a one-file edit in a flat
  directory of 100,000 entries decodes that directory's record. That shape is
  measured separately and not held to the contract; a directory representation
  change is a TODO.md item if it matters.

Server (`crates/wheat-server/tests/`): the view forgery, a forged revision and an
out-of-order view are rejected on publish with a reason. CLI
(`crates/wheat-cli/tests/`): a fetch from a server holding a forged state (written
directly into the server's store) is refused and the local head doesn't move; a
publish whose joined state returned by the server is forged is refused and neither
the local head nor `last_fetched` moves.

Benchmarks: `crates/wheat-bench/src/quick/transition_validate.rs` becomes a contract
on objects and bytes decoded for a one-change publish at 500 vs 5,000 changes and
1,000 vs 100,000 files in directories (≤ 1.25×), plus the flat-directory shape
recorded without a contract, wall time recorded; a case for `wheat labels` /
`wheat frontier list` at the same sizes; `apply_step`'s order-index cost at 100 vs
100,000 changes (logarithmic). Release run; results under `bench/results/`.

## Docs

`docs/design/wheat-native-protocol.md` (what publish and fetch validate),
`docs/design/server-native-source-control-store.md` (the invariants and validation
model), `docs/design/wheat-storage-efficiency.md` (content maps replace content-set
snapshots and deltas).

## TODO.md

Closed by this: "Content manifests are full snapshots"; "Read-only `wheat` commands
cost a full validation" (partly: store open still re-reads the content pack; that
stays open). "Transition validation reads every added blob in full": replay reads the
content it needs anyway; close or keep per Bart.

## Order of work

1. Canonical radix encoding (I9): the per-node check on every radix read, an updater
   that writes nothing non-canonical, updater/builder parity tests. Done.
2. Content maps: format, store, every user (including core, hydration, transfer,
   Git accumulator), full validation; tests (behaviour unchanged, manifests now cost
   the diff). Done. Full validation checks the base view's map against its tree and
   every other view's map against its first lineage parent's by the tree diff,
   falling back to a full comparison when a diff is too large. Import's resume
   authenticates each content-map node once rather than each view's whole map.
3. Revision `combines` and `generation` with every traversal; `join_revisions`
   always authoring one combined revision; view `size`, `last`, `weight`; the weight rule in full
   validation. Done. Full validation checks each view's order against its composition
   parent (the target of the application of `last`) rather than recomputing it from
   the view's maps; step 5's map check on that edge completes the induction. A
   combined revision replays like a reapplied one (an edit may find its change already
   made, never a conflict), and `join_revisions` refuses more than 65 lines at once.
4. Application order index: format, `apply_step`, full validation of I2; tests
   including the canonical-order cases. Done. I2 is a property of an application
   (its change last on its target), so full validation checks it, with the recomputed
   index, once per retained application. An index whose entry counts don't fit its
   position is refused on decode, so a walk never indexes past its end. The index
   entries are references in the object graph.
5. Full validation tightened (partition equality, I1 for every view, composition
   edges by maps). Done. Partition equality: a predecessor's edits are exactly the
   transition's preserved and removed edits, so an edit can no longer be called added
   when its predecessor had it. I1: the base selects nothing, and every other view
   has an edge from its composition parent, its last application's result tree, and
   maps that differ from the parent's by exactly its last change (two map diffs that
   stop at a second difference). Before, a view could select changes beyond its
   parent's and its last, and a view reached only through another parent was never
   checked against its last application. Composition edges by maps: an application's
   edge goes to the target's child whose `last` is its change and which selects it,
   in full validation and in rewrite's `produced_view`. By tree, a join's edge over
   two changes, the second conflicted and leaving the tree as it was, also matched,
   so a fetched join failed validation.
6. `increment_validation.rs`: steps 1, 2, 6-8 and the lineage map diff; then steps
   3-5; forgery and equivalence tests. Done, as `validate_increment(old, new)`
   without modes or a budget yet (step 7). The per-object checks (replay, revision,
   application, placement, composition, content maps) live in `semantic_checks.rs`
   and are shared with full validation, behind a `Membership` trait: full validation
   answers it from its scratch index, the increment from the new state's collections.
   The validator is all-or-nothing, so new views are checked in any order: each
   against its composition parent, old or new, whose own check stands independently.
   Every forgery in `view_order_validation.rs` runs against both validators and must
   get the same verdict; `increment_validation.rs` checks every step of a built
   history (conflicts, resolve, revise, integrate) and the increment-only forgeries
   (history dropped, change-map entries rewritten or misfiled, an application
   composing no view, labels on unretained views). `MAX_LABELS` (10,000) moved into
   core and is enforced on decode, and a union over it is refused, so step 8 never
   reads an unbounded manifest. The scratch index sits behind a mutex so validation
   futures are `Send` for the server.
   Measured read counts grow logarithmically, not by ≤ 1.25×: a one-change increment
   after 40 changes read 65 objects, after 400 changes 100 (radix depth on each
   collection, and the order index's skip chain). The read contract in step 10 needs
   restating as logarithmic.
   Tips on views (`ViewRecord.tips`, a `ChangeSetNode` radix set of ChangeIds): the
   changes nothing else in the view depends on, kept by every composition step and
   checked by the shared composition check as above, so a new change on top of a view
   finds its dependencies without loading the view's history. Done.
7. Forced fallback, bootstrap mode, budgets. Budgets done (3a518b6):
   `validate_increment(old, new, budget)`. Finding what was added (the retained, map
   and lineage deltas and the new view records) is unmetered, since it reads in
   proportion to what was added; checking it reads through a store that refuses reads
   past `objects_per_unit × units × bits(history size)`, at least 8 bits. A unit is an
   added object or an edit that a new revision, or a new application's revision,
   replays, so an application replaying a large old revision pays with its edits.
   `IncrementBudget::STANDARD` (8 per unit and bit) is about eight times what a
   one-change publication reads (82 to 110 reads for 8 units after 1,000 to 100,000
   changes). Over budget: `RepositoryPublicationError::OverBudget`. Bytes and
   content reads are not metered.
   Forced needs no new validator: `validate_envelope` is full validation plus every
   label. Bootstrap was tried both ways (bench `bootstrap_validate`, release, same
   per-run object cache): full validation 0.99 / 14.1 / 66.8 s at 1,000 / 10,000 /
   30,000 changes, `validate_bootstrap` (the incremental steps from a state holding
   only the base view) 0.49 / 6.6 / 24.4 s, with about the same reads and peak memory.
   Which one clone uses is Bart's call. Both grow faster than the history (1.8 M reads,
   3.2 GB at 30,000), partly the bench's shape: its top directories gain an entry per
   256 changes, so a view's tree diff reads larger directory records.
8. Server publish (`commit`, including joined publishes); client fetch, clone and
   the joined-state fetch after a publish.
9. Drop full validation from the local read paths.
10. Benchmarks, docs, TODO bookkeeping.
