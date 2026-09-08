# Wheat Root-Agent Workspaces — Design

Status: draft, not yet implemented
Owner: bart
Last updated: 2026-09-02

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move completed bullets from Outstanding to Done.

### Done

- [x] **Supporting Wheat primitives.** The prototype has stable `ChangeId`s,
  immutable revisions, content-addressed `ViewId`s, strict guarded edits,
  revision transition lineage, deterministic application/conflicts, and
  compare-and-swap repository publication. These are prerequisites rather than
  an implementation of root-agent workspaces.
- [x] **Incremental explicit authoring proof.** `wheat add` proves that a later
  revision can preserve unaffected edit identities, replace or consolidate
  changed edits, remove undone edits, and retain historical revisions.

### Outstanding

- [ ] **Decision gate — workspace contract.** Fix the identity and lifecycle of
  a root-agent workspace, including whether `WorkspaceId` is durable, how its
  owning `ChangeId` and fixed base `ViewId` are represented, and what survives
  suspension, abandonment, and local-directory loss.
- [ ] **Phase 1 — exact view materialization.** Materialize any retained Wheat
  view as a safe private filesystem tree, with authenticated content, file-kind
  fidelity, deterministic paths, and no dependency on a live Git checkout.
- [ ] **Phase 2 — whole-workspace capture.** Compare a private workspace with
  its fixed base and derive one complete revision of its stable change,
  including additions, deletions, binary files, symlinks, executable bits,
  guarded text edits, transition lineage, and bounded race-safe traversal.
- [ ] **Phase 3 — root-agent integration.** Give every root `gt agent` session
  its own Wheat workspace while keeping the parent and all delegated workers in
  that same workspace. Route typed tools, Bash, indexes, builds, checkpoints,
  suspension, and resume through the workspace root.
- [ ] **Phase 4 — commit and resume.** Make an ordinary agent commit advance
  only its workspace's change from revision N to N+1, then prove a restarted
  session reconstructs the committed result and commits further work as the
  next revision of that same change.
- [ ] **Phase 5 — explicit stacking and integration.** Start dependent work
  only when requested from another change's result; compose independent changes
  later against a selected target and retain clean integration views or durable
  conflicts without rewriting their authored revisions.
- [ ] **Phase 6 — physical workspace optimization.** Qualify ordinary private
  materializations first, then optionally introduce shared immutable lower
  trees plus copy-on-write filesystem layers without changing Wheat semantics.
- [ ] **Phase 7 — concurrency and recovery corpus.** Cover isolated simultaneous
  root agents, shared parent/worker mutation, concurrent commit attempts,
  suspension and process death, stale publication retries, explicit stacks,
  integration conflicts, and cleanup of abandoned physical workspaces.
- [ ] **Open question — uncommitted suspension policy.** Decide whether every
  suspended root-agent workspace retains its mutable filesystem indefinitely,
  snapshots uncommitted state into a non-revision workspace object, or requires
  an explicit checkpoint before its physical directory may be reclaimed.
- [ ] **Open question — command naming.** Decide whether agent-facing `commit`
  maps directly to `wheat commit`, is mediated by the agent harness, or uses a
  compatibility command while Git remains available.

## Why this exists

Running several coding agents in one ordinary checkout makes concurrent work
visible before its author intends to share it. One agent observes another's
half-written file, build and search results depend on timing, and overlapping
writes must be manually coordinated. Git worktrees avoid some interference but
make each agent manage Git branches, indexes, object-database relationships,
and directory lifecycles that are incidental to the intended collaboration
model.

Wheat can make isolation a property of its workspace and change model instead.
Every root agent starts from one exact immutable view and owns one stable logical
change. Its filesystem shows that view plus only its own accumulated work.
Committing creates or revises that change; it does not expose mutable files to
other independent agents and does not integrate the change into `main`.
Integration, conflict creation, and target publication happen later and
explicitly.

## Goals

- Give every root agent a repository view unaffected by concurrent independent
  agent activity.
- Make the root workspace a mutable projection of one stable Wheat `ChangeId`
  based on one exact `ViewId`.
- Turn each commit into a new immutable revision of that same change, preserving
  incremental edit identity and historical revisions.
- Reconstruct a committed root-agent workspace from Wheat state when an agent is
  resumed, without relying on an old process or Git worktree.
- Let the root agent and its delegated workers collaborate in one filesystem
  workspace and contribute to the same next revision.
- Require explicit dependency/stack creation before one root agent sees another
  change in its starting tree.
- Defer conflicts between independent changes until an explicit composition or
  integration operation.
- Keep copy-on-write filesystems, snapshot directories, and lazy materialization
  as interchangeable physical strategies below one semantic contract.

## Non-goals

- A separate workspace, change, or handoff bundle for each delegated worker.
- Automatically applying a worker result back into its parent. Workers already
  share the parent's root workspace and their writes are ordinary uncommitted
  workspace mutations.
- Automatically merging every committed agent change into `main`.
- Showing a root agent changes committed elsewhere after its base was selected.
- Treating uncommitted editor or tool operations as canonical Wheat edits. The
  explicit commit boundary captures their accumulated effect.
- Defining the eventual forge UI, review policy, merge queue, or protected-target
  rules beyond the seams this workspace model requires.
- Requiring overlayfs or any other particular filesystem mechanism in the first
  implementation.

## Terminology

The following names distinguish stable collaboration identity from snapshots
and physical directories:

- **Target** — a named shared pointer such as `main`, resolved to an exact
  `ViewId` when a new independent workspace begins.
- **View** — an immutable content-addressed repository state. A `ViewId` commits
  to its tree, selected revision frontier, applications, unresolved conflicts,
  and required visible content.
- **Change** — one stable logical unit of intent, identified by `ChangeId`, whose
  current content can evolve through immutable revisions.
- **Revision** — one immutable complete version of a change. A revision records
  its predecessor, authored base, selected edits and transition lineage, and
  exact resulting tree.
- **Root-agent workspace** — the mutable working-copy projection owned by one
  root agent session lineage. It is associated with one change and one fixed
  base view. The root agent and all delegated workers share it.
- **Physical workspace** — the directory, filesystem mount, or copy-on-write
  layer implementing a root-agent workspace at a particular moment. It is
  disposable once all state promised by the workspace lifecycle has been
  retained elsewhere.
- **Result view** — the view produced by applying a workspace's current revision
  to its declared base/dependency view. This can be selected explicitly as the
  base of a dependent change.
- **Integration view** — a view produced later by composing selected independent
  or dependent changes against a target.

`RevisionId` and `ViewId` must remain distinct. Informal phrases such as "V2 of
the agent's change" mean its second immutable revision, not a mutable or
sequentially numbered repository view.

## Core model

### One root workspace owns one change

Starting an independent root agent resolves the selected target once:

```text
main target -> ViewId M

create_root_workspace(target = main)
  -> WorkspaceId W
  -> ChangeId A
  -> base ViewId M
```

The workspace tree begins as exactly `M.tree`. The association is stable:

```text
RootWorkspace W {
    repository
    owner/session lineage
    change: A
    base_view: M
    current_revision: none | A1 | A2 | ...
    materialized_tree
    lifecycle state
}
```

`main` may advance after creation, but W does not follow it. Another independent
agent resolving `main` at the same moment receives its own workspace and change:

```text
Workspace WA: base M, change A
Workspace WB: base M, change B

WA sees M + uncommitted A work
WB sees M + uncommitted B work
```

Neither workspace sees the other's mutable files or later revisions. If the two
agents start after different target resolutions, their distinct base `ViewId`s
make that difference explicit.

### Delegated workers share the root workspace

Delegated workers do not create another isolation or source-control boundary:

```text
Root agent A
  Workspace WA / Change A
    parent agent
    worker 1
    worker 2
```

Every process in that root-agent tree receives the same workspace root. A
worker's edit, generator invocation, or formatter output changes the same
uncommitted filesystem that the parent sees and that the next commit captures.
The parent remains responsible for partitioning simultaneous tasks that touch
that shared mutable state.

The implementation must nevertheless serialize the commit transaction. Two
processes must not concurrently capture one filesystem and both claim to be the
unique successor of the same current revision. A commit takes an expected
workspace generation/current revision and either advances it once or reports a
stale attempt for retry.

### A commit revises the stable change

The first commit captures the complete accumulated effect relative to the
workspace's fixed base:

```text
commit_workspace(W, expected_revision = none)
  -> Revision A1 {
       change_id: A
       predecessor: none
       authored_against: M
       edits: complete current effect of WA
       resulting_tree: current WA tree
     }
```

Further editing and committing produces a successor of the same logical change:

```text
commit_workspace(W, expected_revision = A1)
  -> Revision A2 {
       change_id: A
       predecessor: A1
       authored_against: M
       edits: complete current effect of WA
       resulting_tree: current WA tree
     }
```

A2 is the latest complete meaning of change A, not merely an imperative patch
to replay after A1. Incremental authoring compares the current workspace with
the authored base and the predecessor revision so that:

- unaffected canonical edits retain their IDs;
- modified replacements or guards become new edits;
- interfering components consolidate with explicit lineage;
- completely undone effects disappear from A2;
- the transition records preserved, added, removed, subsumed, and resolution
  relationships;
- A1 remains retained and inspectable.

Committing updates only the private pointer for workspace W/change A. It does
not advance `main`, compose A with sibling changes, or make A's mutable
filesystem visible to other root agents.

### Resume follows change identity

Restarting an agent on workspace W or change A resolves the latest committed
revision, for example A2, and reconstructs:

```text
base M + current revision A2 -> A2.resulting_tree
```

The resumed root agent receives that tree in a workspace still associated with
A. Its next commit produces A3. Conversation history may help the agent reason,
but the durable source state is the workspace/change/revision/view relationship,
not the conversation transcript or an old checkout directory.

Committed work must therefore survive loss of the physical workspace. What to
do with uncommitted state across suspension is deliberately an open lifecycle
decision: retain the physical directory, snapshot a non-canonical workspace
state, or require an explicit checkpoint.

### Stacking is an explicit start operation

Independent root agents start from a named target's latest view, not from every
currently known agent change. To build B on A, the caller explicitly chooses A's
current result:

```text
create_root_workspace(from_change = A)
  resolves A's current RevisionId and result ViewId MA
  -> Workspace WB
  -> ChangeId B
  -> dependency A
  -> base/dependency view MA
```

B now sees A because that relationship was requested and is recorded in Wheat's
dependency graph. If A advances later, Wheat can reapply B's unchanged revision
on the new dependency result and retain either a clean application or structured
conflict. No live filesystem is shared between the independent root workspaces.

## Integration happens later

Several agents can commit valid independent changes against M:

```text
A1 authored against M
B1 authored against M
C1 authored against M
```

These commits need not conflict merely because their eventual effects overlap.
They are independent authored facts. A later operation selects a dependency
order or integration set:

```text
compose_changes(base = current main, changes = [A1, B1, C1])
  -> clean IntegrationView
  |  IntegrationView with durable structured conflicts
```

Wheat applies the immutable revisions against the chosen target and records the
actual applications. Strict guards decide whether an edit remains valid; Wheat
does not silently fuzzy-match, mutate the authored revisions, or resolve
ordering ambiguity by observing which agent wrote first.

If the shared target advances before integration publishes, the integration
operation retries against the new exact target view. It reuses the same
immutable agent revisions and creates new applications; it does not recapture
the agents' workspaces.

This separates three events that Git-oriented workflows often conflate:

1. **Workspace commit** — retain revision A2 and advance only workspace/change A.
2. **Composition** — apply selected changes to construct an integration view and
   durable conflicts if necessary.
3. **Target publication** — advance `main` or another named target through policy
   and compare-and-swap fencing.

## Workspace operations

The intended workflow-level interface is approximately:

```text
create_workspace(target, owner, idempotency_key)
  -> WorkspaceId, ChangeId, base ViewId

create_workspace_from_change(change, revision?, owner, idempotency_key)
  -> WorkspaceId, ChangeId, dependency/result ViewId

open_workspace(workspace)
  -> workspace metadata and materialized root

commit_workspace(workspace, expected_revision, expected_workspace_generation)
  -> new RevisionId, result ViewId, new workspace generation

suspend_workspace(workspace, expected_generation)
resume_workspace(workspace | change)
abandon_workspace(workspace, expected_generation)

compose_changes(base_view, ordered/dependency-selected revisions)
  -> IntegrationViewId and applications/conflicts

publish_target(target, expected_view, integration_view)
  -> publication receipt
```

These are domain operations, not settled CLI spellings. Remote forms need
idempotency keys and bounded resumable content transfer. Mutating operations
must authenticate ownership/authorization and use explicit expected identities
rather than relying on process-local locks.

## Whole-workspace capture

The current explicit-path `wheat add` is a useful algorithm proof but not the
root-agent commit boundary. `commit_workspace` must capture the entire relevant
working tree deterministically.

For every path in the union of the fixed base tree and physical workspace, it
must classify:

- unchanged entries;
- text changes eligible for guarded authoring;
- file creation and deletion;
- binary or symlink replacement;
- regular/executable/symlink kind changes.

The capture must authenticate baseline content from Wheat storage and filesystem
content through safe no-follow traversal. It must reject path escapes, special
files, unsupported metadata, racing replacements, and configured size/work
limit violations. Content is stored before the revision and result view become
reachable.

A successful capture must prove:

```text
apply(revision.edits, revision.authored_against.tree)
    == revision.resulting_tree
    == authenticated workspace tree at the commit boundary
```

The filesystem must not be writable by a still-running process during the final
capture window unless the implementation can detect and reject every relevant
race. A practical first implementation can take a short workspace mutation
lock, stop new tool dispatch, wait for registered foreground/background
commands, capture, then release the workspace to further edits.

## Physical materialization

The semantic contract does not require Docker-style layers. A first version may
materialize a full private source directory for every root workspace because it
is easier to qualify. Build outputs and dependency caches need a separate
policy; they need not be canonical workspace content merely because they live
under the process's current directory.

Once semantics and capture are proven, physical implementations may include:

- cached immutable directory trees keyed by `ViewId` plus a private writable
  copy;
- a read-only image such as SquashFS plus a private overlayfs upper layer;
- lazy userspace materialization backed by Wheat content storage;
- platform-specific clone/reflink copies where supported.

In every case the lower source state must be immutable. Using the live ordinary
checkout as an overlay lower directory is invalid because later host mutations
would leak into the agent view. Raw overlay upper directories are physical
metadata, not canonical Wheat revisions; commit captures the authenticated
merged tree through the same workspace-capture contract as any other backend.

Disposable build caches may be shared for performance only when doing so cannot
change source visibility or correctness. A shared cache is not part of the
workspace's logical tree, and cache corruption or eviction must not change a
revision's identity.

## Agent integration boundary

A root `gt agent` session must be constructed with the workspace's materialized
root before its runtime and tools are created. All repository consumers must use
that root consistently:

- typed read/write/search tools;
- Bash and every descendant process;
- semantic and symbol indexes;
- builds and tests;
- filesystem policy evaluation;
- checkpoints and session resume metadata.

Delegated workers inherit the root agent's workspace unchanged. Read-only
workers may share it too; their reads intentionally include current uncommitted
parent/worker mutations because they are collaborators within one root change.

The agent session record should retain `WorkspaceId`, `ChangeId`, fixed base
`ViewId`, and current `RevisionId`/workspace generation. Merely persisting a
conversation and the original process CWD cannot reconstruct the intended
source state.

An ordinary agent instruction to "commit" commits the Wheat workspace change.
It must not implicitly integrate into `main`. While Git compatibility remains
available, the harness must make the distinction explicit so a Git commit in an
incidental projection cannot become the canonical source state by accident.

## Concurrency and failure semantics

### Independent roots

Root workspaces never read one another's mutable directories. Additive content
and immutable-object writes may occur concurrently. Updating a private
workspace/change pointer uses an expected revision/generation and cannot clobber
a winner.

### Shared parent and workers

Parent and worker filesystem writes have ordinary same-workspace race semantics;
task partitioning remains necessary. File tools may keep per-path locks, while
arbitrary commands require the root agent to avoid overlapping destructive
operations. The commit boundary additionally serializes against all workspace
mutations it can supervise.

### Process death

A crash before commit must not create a reachable revision. A crash after the
immutable closure is durable but before the workspace pointer advances may
leave harmless unreachable objects and must be retryable with the same
idempotency key. A crash after pointer publication must reopen the exact new
revision and never repeat it as another successor.

### Stale integration

A change committed against its private fixed base remains valid even when
`main` moves. Staleness is resolved during composition/application. A target CAS
failure retries composition against the winner's new view and preserves the
original revision identity.

### Abandonment and reclamation

Abandoning a workspace removes its private mutable pointer and eventually its
physical materialization. Committed changes and revisions follow repository
retention policy and are not deleted merely because the creating workspace was
abandoned. Uncommitted physical state follows the separately chosen suspension
and abandonment policy.

## Required qualification corpus

At minimum, black-box tests must demonstrate:

1. Two root agents opened from M receive distinct workspace and change IDs.
2. A write in WA is invisible in WB and in the shared target view before commit.
3. A1 and B1 both commit successfully against M even when they modify the same
   path.
4. Composing A1 and B1 later yields the deterministic clean result or durable
   guarded-edit conflict required by their contents and selected order.
5. A second commit in WA creates A2 with predecessor A1, preserves unaffected
   edit IDs, and removes an effect that was undone in the workspace.
6. Destroying WA after A2 and resuming A reconstructs A2's exact resulting tree;
   the next commit is A3, not a new change.
7. An explicitly stacked B starts from A's selected result view and records A as
   a dependency; an ordinary independent B does neither.
8. Advancing A after B was stacked causes deterministic reapplication of B and
   retains any resulting conflict without rewriting B's authored revision.
9. Parent and delegated worker edits in one root session are captured together
   in the same next revision.
10. Concurrent parent/worker commit attempts cannot create ambiguous sibling
    successors from one workspace generation.
11. A process dying at each content/object/pointer publication boundary either
    leaves the old revision current or reopens the exact committed successor.
12. A target advancing during later integration causes a CAS retry using the
    same agent revisions, never filesystem recapture.
13. Losing or reclaiming a physical workspace cannot lose committed revisions.
14. Every qualified physical backend produces the same canonical revision and
    result view from the same base and final filesystem tree.

## Phasing

### Phase 0 — settle the workspace contract

Resolve the outstanding lifecycle and identity questions. Extend the
experimental schema in place while its lock decision remains open. Define
workspace ownership, expected-generation semantics, committed versus
uncommitted durability, and the relationship between workspace, change,
revision, result view, and named target.

### Phase 1 — materialization and capture library

Add safe view-to-filesystem materialization and deterministic whole-workspace
capture beneath a library interface independent of `gt agent`. Exercise it
against ordinary private directories first and prove canonical parity with
explicit per-path authoring for equivalent changes.

### Phase 2 — reference Wheat CLI workflow

Expose create/open/commit/resume/abandon operations in the standalone client.
Prove multiple independent workspaces, repeated revisions, explicit dependent
workspaces, process restart, and later composition without any agent-specific
behavior.

### Phase 3 — root-agent adoption

Associate one Wheat workspace with each root agent session lineage and route the
root plus delegated workers through it. Persist workspace identity in local and
remote resume state. Keep independent root sessions isolated and ensure commit
advances only the owning change.

### Phase 4 — optimize physical workspaces

Measure full materialization cost and introduce copy-on-write/lazy backends only
where justified. Backend qualification must compare canonical capture outputs
and filesystem behavior against the ordinary-directory reference backend.

### Phase 5 — forge integration

Add first-class private change targets, browsing/review/CI attachment,
composition controls, target policy, publication receipts, retention, and
administrative cleanup. Those product surfaces build on the workspace contract;
they do not redefine it.

## Open questions for review

1. Is `WorkspaceId` a durable repository-domain identity, a client-local lease,
   or a durable session object outside the canonical repository graph?
2. Must suspended uncommitted work survive loss of the client machine? If so,
   is it represented as a workspace snapshot distinct from a reviewable
   revision?
3. Does reopening by `ChangeId` select its one active workspace, create another
   physical attachment to it, or require an explicit `WorkspaceId` when several
   local copies exist?
4. Does the fixed base of a revision remain M for every revision of A, or should
   an explicit operation be able to rebase/re-author A onto another view and
   thereby create a new revision with transition evidence? Automatic movement is
   excluded either way.
5. How are generated and ignored files classified into canonical workspace
   content, disposable build output, and shared caches without importing Git's
   ignore semantics as Wheat's permanent model?
6. Which process and filesystem controls are required to make whole-workspace
   capture race-free when agents can launch arbitrary descendants?
7. During the Git-compatibility period, what exact command/UI distinction keeps
   a canonical Wheat commit separate from a commit made only in a disposable Git
   projection?
8. Can more than one root session intentionally attach to the same workspace and
   change, or is that rejected in the first version to preserve a single clear
   owner?

## Relationship to the main Wheat design

This document refines the `workspace` and concurrent-agent portions left open by
[`server-native-source-control-store.md`](./server-native-source-control-store.md).
That document remains authoritative for canonical changes, revisions, guarded
edits, applications, views, content retention, publication, storage, protocols,
and Git projection. If this workflow requires those primitives to change, the
main design must be updated rather than allowing two contradictory definitions.

The present prototype already supplies useful evidence:

- `RepositoryView` is an immutable content-addressed snapshot.
- `ChangeId` is stable while revisions are immutable and content-addressed.
- revision creation records predecessor, dependencies, authored view, complete
  edit selection, transition lineage, and resulting tree;
- applying a revision to another target produces exact clean or structured
  conflicted evidence;
- repository publication uses expected identities and durable CAS;
- `wheat add` demonstrates incremental revision authoring but is intentionally
  explicit-path and tied to the current published application.

It does not yet have `WorkspaceId`, exact-view filesystem materialization,
whole-tree commit capture, detached private change pointers, root-agent session
binding, or the lifecycle operations specified here.

## References

- [`docs/design/server-native-source-control-store.md`](./server-native-source-control-store.md)
- `crates/wheat-core/src/model.rs` — changes, revisions, applications, and views
- `crates/wheat-core/src/repository.rs` — revision, dependency, composition, and
  publication reference model
- `crates/wheat-core/src/edit_capture.rs` — pure authenticated before/after
  capture
- `crates/wheat-core/src/guarded/authoring.rs` — incremental guarded-edit
  normalization and lineage
- `crates/wheat-cli/src/workspace.rs` — current explicit-path workspace authoring
  proof
- `crates/wheat-cli/src/workspace_files.rs` — bounded safe workspace reads
- `crates/wheat-store/src/publication.rs` — repository-head CAS
- `crates/wheat-store/src/repository_publication.rs` — immutable closure
  preparation and publication
