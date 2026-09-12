# Wheat Local CLI — Design

Status: partial — low-level repository and change workflows exist; editable workspace workflow remains outstanding
Owner: bart
Last updated: 2026-09-09

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land; move items from Outstanding to Done rather than leaving completed work in
both lists.

### Done

- [x] **Repository lifecycle plumbing.** `wheat init`, `status`, `validate`, `log`,
  `cat`, and reopen support operate against loose and packed local repositories.
- [x] **Explicit native change mutation.** `change new`, explicit-path `add`,
  immutable revision predecessors, dependency declaration, descendant
  reapplication, and durable head publication are implemented.
- [x] **Conflict evidence and resolution.** `conflicts`, `edits`, and `resolve`
  expose retained edit/application evidence and publish exact resolutions.
- [x] **Git bootstrap and publication compatibility.** `wheat import` accepts a
  local Git repository or temporary-mirrors a Git URL into a fresh packed Wheat
  repository; `wheat push` projects a selected clean native view and publishes
  one Git branch with exact force-with-lease semantics.
- [x] **Linux workspace safety and materialization.** Explicit regular,
  executable, and symlink reads use descriptor-relative no-follow traversal;
  authority paths are reserved. Separate stores are linked from workspace-local
  metadata. Fresh workspaces materialize one pinned view, and existing workspaces
  move incrementally between published, working, and named-branch views using
  bounded Merkle diffs, exact-preimage transactions, crash recovery, and
  kernel-enforced no-replace/exchange operations. Changed or obstructed workspace
  paths fail closed without overwriting user data.
- [x] **Tracked workspace status foundation.** Human and versioned JSON status
  compare tracked paths with the pinned materialized tree, distinguish content,
  kind, and deletion changes, treat canonical-equivalent line endings as clean,
  and report repository advancement. Diff rendering and conflict-path expansion
  remain outstanding.
- [x] **Selected-path restore.** Explicit file and directory selectors authorize
  restoring their tracked leaves from the pinned materialized tree. Workspace-only
  paths and untracked siblings are preserved, exact observed preimages close
  concurrent-write races, and durable terminal records complete valid plans or
  consume invalidated consent while retaining completed work. Restore never changes
  the selected view or workspace metadata.
- [x] **Tracked workspace diff and conflict paths.** `diff` compares tracked workspace
  paths with the pinned materialized tree, a retained revision with its predecessor,
  or two retained views. Versioned JSON and Git-familiar human output share bounded
  descriptors and contextual hunks; binary, symlink, oversized, over-complex, and
  deadline-limited bodies remain explicit opaque metadata. Initial revisions compare
  against their `authored_against` view. `conflicts` expands retained edit identities
  to canonical affected paths while preserving exact failure evidence and never
  fabricating unavailable ranges. Untracked/ignored scanning remains Phase 2 work.
- [x] **Native change/revision metadata.** Native changes derive their stable title
  from the required revision message and may carry an explicit description;
  revisions retain messages, actor attribution, and local or explicit RFC3339
  timestamps with offsets. A versioned bounded config at the separate store and
  XDG user scope supplies one default actor, with repository values taking
  precedence per key. `wheat config set [--global] actor.name|actor.email VALUE`
  updates it atomically. Phase 1 deliberately has no editor, identity environment
  variables, or distinct author/committer overrides.
- [x] **Ergonomic selected-path capture.** Each workspace retains an immutable
  authoring basis and one selected active logical change. `wheat changes` lists the
  basis-relative revision delta, `change new` selects its new identity, and
  `add [--change ID] PATH...` resolves the exact predecessor without copied IDs.
  Explicit adds report their destination while preserving the workspace selection.
  Descendant edits already absorbed by an amended ancestor reapply as authenticated
  no-ops rather than false conflicts. Workspace-wide capture remains Phase 2 work.

### Outstanding
- [ ] **Phase 1 — cross-platform workspace contract.** Provide equivalent
  non-Linux safety/file-kind guarantees or continue to fail closed for workspace
  mutation on unsupported platforms.
- [ ] **Phase 1 — structured automation contract.** Version machine-readable
  outcomes and error categories, stabilize exit behavior, bound diagnostics, and
  cover retryable stale-state outcomes in black-box tests.
- [ ] **Phase 2 — branch and target workflow.** List, create, rename, delete, and
  switch named targets; define which target mutations advance and materialize it
  without treating a mutable name as change identity.
- [ ] **Phase 2 — dependency and integration workflow.** Expose stack revision,
  dependency movement, joins, deterministic descendant reapplication, and durable
  conflict outcomes as Wheat operations rather than Git-shaped rebase state.
- [ ] **Phase 2 — ignore rules.** Define repository and user ignore configuration
  before workspace-wide status/capture can include untracked files safely.
- [ ] **Phase 2 — lifecycle tooling.** Choose and implement the contract for
  formatters, linters, policy checks, and notifications around capture, revision,
  materialization, and publication.
- [ ] **Phase 2 — repository discovery and local configuration.** Discover a
  repository from descendants and define configuration for identity, editor,
  pager, defaults, aliases, and compatibility remotes without making Git config
  authoritative.
- [ ] **Phase 3 — client/protocol boundary.** Introduce `wheat-client` and
  `wheat-protocol`, move local workspace operations behind the client contract,
  and remove the CLI's direct dependence on production store capabilities before
  adding remote mode.
- [ ] **Phase 3 — local synchronization.** Add persisted remote/target metadata
  and incremental synchronization once the versioned native protocol exists;
  one-shot Git import remains a compatibility bootstrap, not native clone/pull.
- [ ] **Phase 3 — multiple workspaces.** Define workspace identity, concurrent
  local workspaces, cleanliness checks, and content pins without copying Git's
  index/worktree coupling by default.
- [ ] **Open question — lifecycle tooling shape.** Choose Git-compatible
  executables, declarative repository tasks, a structured versioned hook
  protocol, or a deliberate combination.
- [ ] **Open question — workspace model and metadata location.** Decide whether
  Wheat needs a durable workspace record at all and, if so, whether it contains
  repository authority, points at a separate store, or is a named object within
  a local client database.
- [ ] **Open question — publication-target model.** Close the parent design's
  canonical-primitives gate for named publication, target policy, detached views,
  and workspace association before making branch-like commands normative.
- [ ] **Open question — staging semantics.** Decide whether Wheat needs a
  persistent index-like state or whether explicit change/revision capture makes
  one unnecessary.
- [ ] **Defer — signing and verification.** Design native attestations only after
  actor identity and publication evidence are fixed; do not copy opaque Git
  signature headers into native identity.
- [ ] **Defer — advanced history utilities.** Blame, bisect, archive, grep, and
  history rewriting follow the complete edit/materialize/integrate loop.

## Why this exists

The server-native source-control design defines Wheat's canonical collaboration
and storage semantics. It deliberately does not derive those semantics from a
Git command vocabulary. That separation is necessary, but it leaves a second
question large enough to deserve its own contract: can a person use Wheat for
ordinary local development without manually assembling internal object IDs or
falling back to Git for every workspace operation?

The reference CLI now proves durable storage, stable logical changes, revision
application, conflict retention, Git import/projection, safe materialization,
tracked status and diff, native attribution, and selected-path authoring without
copied object IDs. The remaining Phase 1 boundary is a stable structured
success/error contract; workspace-wide untracked capture, named targets, and
native synchronization deliberately remain later phases.

This document specifies the local client experience. The parent architecture
remains [`server-native-source-control-store.md`](./server-native-source-control-store.md).
The future native wire protocol must expose the same workflow semantics, but its
transport, authentication, and repository-daemon design remain in the parent
document.

## Goals

- Make a fresh native repository or Git import immediately usable as an editable
  workspace.
- Make the safe/default path concise while retaining explicit plumbing commands
  for diagnosis and scripting.
- Preserve stable logical change identity across revisions, dependency movement,
  integration, and Git projection.
- Never overwrite unrecorded workspace content, follow attacker-controlled
  symlinks, or publish a partially materialized state.
- Make conflicts inspectable and editable as durable native outcomes rather than
  temporary process state.
- Let automation run deterministically with structured results and bounded work.
- Keep local and future remote modes semantically aligned: latency and transport
  may differ, but the user-visible operations and stale-state outcomes must not.
- Keep Git import/export at the compatibility boundary. Git refs, indexes,
  commit OIDs, and config must not become hidden native authority.

## Non-goals

- Reproduce every Git command, flag, hook name, or index behavior.
- Preserve compatibility with experimental Wheat repository formats before the
  native schema-lock decision.
- Implement native network synchronization in this local-CLI phase.
- Treat a Git projection as a backup of Wheat-only identities or evidence.
- Invent signing semantics before native actors, attestations, and publication
  receipts have a stable contract.
- Make workspace-wide filesystem scans unbounded or silently capture every file.

## Principles

### Commands express Wheat concepts

Where Git's terms fit the user intent—`status`, `diff`, `branch`, `push`—using
familiar names is useful. Where they encode Git's storage accidents—index,
detached HEAD, rebase todo files, force-updating commits—the CLI should expose
Wheat concepts directly: logical changes, immutable revisions, dependencies,
applications, views, conflicts, and publication targets.

### Readable defaults, explicit stale-state handling

Routine commands should discover the repository and current workspace, but every
mutation must resolve those conveniences to pinned IDs before work begins. A
stale predecessor, changed repository head, or changed workspace file produces a
structured refusal and retry guidance. It must never silently retarget a
revision to whatever became current.

### Workspace bytes are not repository authority

The workspace is an editable projection of a pinned native view plus local
changes. Native objects and the repository head remain authoritative. Filesystem
mtime and size may accelerate status, but content and file kind determine
correctness.

### One high-level operation, reusable plumbing

Interactive commands should be composed from testable operations also available
to automation: inspect status, plan materialization, verify a plan, capture a
revision, apply dependencies, and publish a target. Human-readable output is a
view over structured outcomes, not the only API contract.

## Candidate workspace model

One candidate is an explicit local workspace record. This is not yet a settled
canonical primitive; the parent design's workspace decision gate remains open.
If selected, the record would identify at minimum:

```text
Workspace {
    repository_identity
    selected_target?       // named target, not canonical change identity
    materialized_view
    expected_repository_head
    sparse_paths?          // deferred unless needed early
}
```

If this model is selected, the record must not claim that the filesystem still
matches `materialized_view`; status verifies that. Its publication must be atomic
with respect to a completed materialization, or recoverably journaled so an
interrupted checkout cannot masquerade as clean. Alternatives without a durable
workspace record must provide the same pinned-base and interruption guarantees.

### Repository layout decision

Two layouts remain viable:

1. **Embedded authority:** repository storage and workspace metadata live under a
   control directory in the source tree.
2. **Separate authority:** a workspace points to a local repository store, which
   allows multiple workspaces without duplicating packs.

The current loose layout uses `.wheat/`, while packed Git imports currently place
`repository.head` and `storage/` at the destination root. That is useful
prototype evidence but not yet a workspace contract. Materialization should not
cement either layout accidentally; the open question must be resolved first.

## Materialization and restore

Materialization takes a pinned view and an observed workspace state and produces
an authenticated plan before writing. The Linux implementation materializes a
fresh workspace and incrementally moves an existing workspace between published,
working, and named-branch views. A bounded iterative Merkle diff prunes equal
subtrees, so planning visits unequal directory nodes and mutation work is
proportional to changed paths and bytes rather than the complete tree.

Existing-workspace updates retain exact preimages in a private transaction,
stream-authenticate workspace and transaction files with fixed scratch, and use
`RENAME_NOREPLACE`/`RENAME_EXCHANGE` to preserve concurrent data. An immutable
O(delta) journal plus fixed-width append-only progress records supports abrupt
crash recovery. Workspace metadata becomes the commit marker only after every
planned view-switch mutation is durable.

`wheat restore <paths...>` targets the workspace's pinned materialized tree even
when repository authority has advanced. Naming a tracked file or directory is
consent to discard the exact observed values of its selected tracked leaves;
directories recursively select tracked descendants, while workspace-only paths
and untracked siblings are never included. A validated, durably published restore
journal records value-specific intent for the complete invocation. Recovery rolls
that plan forward while not-yet-mutated paths still match their authorized exact
preimages. A later edit invalidates the remaining plan: Wheat preserves that value,
durably aborts and consumes the journal, retains any completed prefix, and requires
an explicit rerun to renew consent. A completed plan writes a durable terminal
record before cleanup and then consumes intent by durably removing the journal.
This journal is recovery state owned by the same workspace principal that can
invoke restore and edit the workspace, not a separate privilege boundary. Exact
raw-byte-and-kind preimages prevent a concurrent writer from being mistaken for
the value the invocation authorized. A clean selection is a true no-op and does
not create recovery state or rewrite metadata.

The plan classifies each path as:

- create;
- replace content;
- change file kind or executable bit;
- delete;
- unchanged;
- blocked by an unrecorded local modification;
- blocked by an unsafe filesystem object or path traversal.

The default operation aborts the entire plan if any path is blocked. An explicit
replacement mode may be added, but it must display or emit the exact paths it
will discard and revalidate immediately before mutation. It must not be a broad
`--force` whose effects change after the plan is shown.

Filesystem requirements:

- operate relative to an opened workspace directory descriptor;
- never follow symlinks in ancestors or the final component;
- create regular files privately, write and sync them, then rename into place;
- create symlinks without following their target and durably sync affected parent
  directories; preserve regular, executable, and symlink kinds;
- remove directories only when empty and owned by the materialization plan;
- reserve repository authority (`.wheat/`, packed `repository.head`/`storage/`),
  projection (`.wheat-git/`), workspace metadata, and journal paths from every
  capture and materialization operation;
- bound path count, individual content size, total bytes, and concurrent writes;
- journal per-path preparation/publication and define whether recovery rolls
  forward or rolls back after a partially visible multi-path update;
- fsync created/renamed/removed entries and each affected parent directory in the
  required order;
- update workspace metadata only after every planned mutation is durable.

Initial commands may be:

```text
wheat materialize [--source published|working|branch:<name>] [DIRECTORY]
wheat restore [PATH ...]          # restore from the pinned workspace view
wheat switch <target>             # target selection plus safe materialization
```

Names are provisional; the safety and pinned-view semantics are not.

## Status and diff

`wheat status` should combine repository state with a workspace comparison. Its
human view should answer:

- which named target and exact view are selected;
- whether working and published views differ;
- which tracked paths are modified, deleted, kind-changed, or conflicted;
- once Phase 2 ignore semantics land, which paths are untracked or ignored;
- which logical change/revision is the default capture target;
- whether repository authority changed since the workspace was materialized;
- which state is local-only versus represented by a configured remote target.

Machine-readable output should be versioned and opt-in, for example
`--format=json`, with stable status categories rather than parsing prose.

`wheat diff` needs three initial forms:

```text
wheat diff                         # tracked workspace vs pinned materialized tree
wheat diff --revision <id>         # revision vs predecessor/authored-against view
wheat diff --from <view> --to <view>
```

Exact retained view IDs and `published`, `working`, and `branch:<name>` selectors
are resolved against one authority snapshot. Text files use canonical-LF
comparison while showing useful context. Binary and symlink changes report kind,
identity, and size without forcing bytes through a text renderer; content above
the inline limit remains deliberately unclassified rather than being mislabeled
binary. Rename detection is presentation evidence, not canonical identity. Tree
walks, content reads, text work, model size, and atomic human/JSON output all have
explicit command-wide limits.

## Capture and revision authoring

The existing plumbing remains useful:

```text
wheat add --change <id> --predecessor <id> PATH...
```

The routine interface resolves the selected change and exact predecessor from
one authenticated head snapshot:

```text
wheat change new [-m "message"]
wheat add [-m "optional revision note"] PATH...
wheat add --change <active-change-id> PATH...
wheat changes [--format human|json]
wheat change select <active-change-id>
```

Exact names remain open, but the behavior should:

1. compute status against the pinned base;
2. select or create one logical change explicitly enough to avoid ambiguity;
3. resolve the expected predecessor revision;
4. capture selected paths using the existing guarded/exact edit machinery;
5. revalidate files and repository head before publication;
6. persist useful revision metadata;
7. report descendant applications and conflicts caused by the revision;
8. leave the workspace untouched unless a separate materialization is requested.

Workspace-wide capture must first have ignore semantics and configured bounds.
It must display or return the selected path set before mutation in interactive
flows. Untracked files are never included merely because they exist beneath the
workspace root.

## Identity, messages, and configuration

Native authoring uses this Phase 1 identity precedence contract:

1. repository configuration in the separate Wheat store;
2. XDG user configuration;
3. otherwise a clear refusal when either required actor field is absent.

Configuration is set one typed key at a time with
`wheat config set [--global] actor.name|actor.email VALUE`; repository values
replace user values per key. One configured actor fills creator, author, and
committer roles. Distinct-role overrides and environment identity are deferred.

`change new -m` is the non-interactive form; without `-m`, a terminal invocation
prompts for one bounded line while non-terminal input refuses rather than blocking.
The first trimmed message line is the stable change title; `change new
--description` supplies its optional long description. `add` needs no repeated
message because the logical change carries enduring intent, but accepts `-m` as
an optional revision note. An absent note remains empty rather than copying the
change title or inventing prose. Change creation records creator and created-at,
while every revision records author, authored-at, committer, committed-at, and
its optional message. Timestamps default to the current local clock and accept
explicit RFC3339 flags, preserving minute offsets. A revision leaves its logical
change's metadata unchanged. `wheat log` shows the current revisions' title,
author, timestamp, and any nonempty revision note.

Configuration files are versioned, bounded, strict JSON and published atomically
under a per-config lock. Repository configuration belongs to Wheat, not
`.git/config`; user configuration is `${XDG_CONFIG_HOME:-~/.config}/wheat/config.json`.
Secrets and credential material must not be copied into repository configuration
or diagnostic output. Editor, pager, default-target, and output-format settings
remain part of the broader configuration work in Phase 2.

## Active workspace changes

A workspace fixes an `authoring_basis` to the exact first materialized view and
stores one optional `selected_change`. Active state is derived, never stamped on
a logical change: a change is active when the working view selects a revision
that is absent from or differs from the basis selection. This makes a Git import's
selected tip the basis rather than incorrectly listing all imported commits above
the repository's empty global base.

`wheat changes` reports this identity-preserving revision delta in deterministic
dependency order and marks the workspace selection. It distinguishes current
published-revision equality from active state, but does not call a change landed,
closed, or withdrawn; those are target-relative lifecycle facts that require the
Phase 2 publication-target contract. Explicit `add --change A` is a one-operation
override and does not replace selected change B.

When amending A causes an unchanged descendant B's intended replacement to be
present already, B's immutable edit is recorded as successfully satisfied against
the authenticated target view. Exact edits require complete `FileValue` equality;
guarded text requires a unique replacement match in the original context. The
application records its target and read evidence with no write. This permits the
common correction of adding a forgotten file to an earlier change while B becomes
a semantic no-op after reapplication. Incompatible or ambiguous effects remain
durable conflicts. Authored revisions stay strict: only descendant reapplication
may accept already-satisfied edits.

Every successful add prints the destination change ID and title after repository
publication, including implicit adds, so a mistaken selection is immediately
visible. Adding to the intended earlier change is the normal correction path; no
mutable revision or imperative transfer is fabricated.

## Candidate named-target and integration workflow

The parent design has not yet closed the canonical publication-target gate. A
candidate model treats a branch-like name as a mutable publication target
pointing to a native view, distinct from change, revision, workspace, and review
identity. If selected, initial target commands should support list, create,
rename, delete, and switch with compare-and-swap updates.

Mutations need an explicit target policy. The current prototype advances working
and published views together for daily mutations and preserves imported branch
manifests without moving them. Before branch commands ship, define:

- whether a workspace follows one target or can be detached at a pinned view;
- whether capture advances a target, only a working view, or both;
- how a clean integration becomes published;
- which targets may point to conflicted views;
- how target deletion interacts with retained reachability.

Stack editing and integration should then expose dependency changes and
application outcomes directly. Revising the bottom of `A -> B -> C` should show
new applications of descendants without pretending that old revisions ceased to
exist. Combining independent tips should create an explicit integration/join
operation with durable conflicts, not an interrupted merge directory.

## Ignore semantics

Ignore rules are required before workspace-wide status and capture. The initial
contract should distinguish:

- repository-versioned rules;
- user-global rules;
- command-local exclusions;
- tracked paths, which remain visible even if a later rule matches them.

Whether Wheat accepts `.gitignore` syntax for compatibility or defines a smaller
native syntax is an open decision. Compatibility is useful, but Git's complete
pattern and nested-negation behavior is a real contract and should not be
claimed partially. Ignore evaluation must be bounded and must not follow
symlinked directories.

## Lifecycle tooling and hooks

Wheat currently has no local hook mechanism. The design should begin from
lifecycle events rather than copying filenames from `.git/hooks`:

- before and after workspace materialization;
- before capture;
- before immutable revision publication;
- after revision publication, including descendant/conflict outcomes;
- before target publication;
- before and after Git compatibility push.

Three implementation shapes are viable:

### Git-compatible executable hooks

Advantages: familiar, trivial for shell tooling, and easy for existing
repositories to adopt. Costs: ad hoc environment contracts, weak composition,
platform differences, difficult structured diagnostics, and inherited bypass
semantics.

### Declarative repository tasks

A versioned Wheat file declares commands for lifecycle events, inputs, timeout,
and failure policy. Advantages: reviewable and portable policy with deterministic
ordering. Costs: command execution and trust still need a sandbox/security model,
and local preferences should not automatically become repository policy.

### Structured hook protocol

Wheat sends a versioned request on stdin and receives structured diagnostics and
fixes. Advantages: stable integration with editors, formatters, forge policy, and
machine-readable outcomes. Costs: more implementation burden for simple shell
scripts.

A combination may be appropriate—for example declarative task registration with
a structured protocol and an explicit shell adapter—but this remains an explicit
owner decision gate. Whichever shape is selected must define timeouts, output bounds,
environment allowlists, ordering, concurrency, trust/approval, failure classes,
and bypass auditing. Hooks cannot be the only correctness or authorization
boundary; repository publication revalidates native invariants independently.

## Git compatibility workflow

During local dogfooding, Git remains the remote compatibility path:

```text
wheat import <local-git-path-or-url> <fresh-wheat-destination>
wheat --repository <wheat-repository> push <git-url> --to refs/heads/main
```

Import from a URL temporary-mirrors the complete Git repository and then deletes
the mirror. It establishes no native remote or incremental-fetch relationship.
Push exports one selected clean view and intentionally loses Wheat-only evidence.
Neither operation is native clone, fetch, pull, or synchronization.

A configured compatibility remote may remove repetitive URL/ref arguments later,
but configuration must identify it as a Git projection endpoint. Receiving
changes from it remains an explicit import/reconciliation operation; a changed
Git ref must not silently redefine canonical Wheat state.

## Repository discovery

Commands should discover a workspace/repository by walking ancestors from the
current directory, stopping at filesystem and explicit safety boundaries. An
explicit `--repository` always wins. Discovery must distinguish a repository
store from an editable workspace so a store nested near source files is not
mistaken for permission to modify them.

## Errors and automation

Routine failures need distinct structured categories:

- workspace dirty or blocked path;
- repository head changed;
- predecessor revision stale;
- target compare-and-swap failed;
- conflict produced;
- content missing or corrupt;
- policy/tool check failed;
- transport or Git projection rejected;
- configured limit exceeded.

Human output should give the safe next operation. Exit codes or structured
results should distinguish retryable stale-state outcomes from invalid input,
policy refusal, and corruption. No command should require scraping a nested
error chain to decide whether retry is safe.

## Phasing

### Phase 1 — complete a single-target edit loop

1. Decide the workspace model and metadata placement.
2. Isolate all repository/projection authority paths and define supported-platform
   safety guarantees.
3. Implement planned, crash-recoverable materialization and selected-path restore.
   Both view switching and pinned selected-path restore are complete.
4. Implement tracked-path workspace status and diff.
5. Add complete native change/revision metadata and identity configuration.
6. Add ergonomic selected-path capture using the current change and predecessor.
   Complete: workspace selection, active listing, explicit one-shot override, and
   absorbed descendant no-op evidence are implemented.
7. Add versioned structured outcomes, stable error categories, and bounded output.
8. Run a black-box Git-import → materialize → edit → diff → revise → push flow,
   including machine-readable retryable failures.

### Phase 2 — multiple targets and repository tooling

1. Define and implement named-target operations and switching.
2. Add workspace-wide status/capture with complete ignore semantics.
3. Add dependency movement and integration commands with durable conflict output.
4. Add repository discovery and typed local configuration.
5. Choose and implement lifecycle tooling/hooks.
6. Qualify concurrent workspaces and stale-head outcomes locally.

### Phase 3 — native synchronization

1. Introduce `wheat-client` and `wheat-protocol`, extract workspace operations
   behind the client interface, and stop the CLI from depending directly on the
   production store implementation.
2. Run the same semantic operations through the versioned native protocol.
3. Add native clone/fetch/publish with resumable bounded object transfer.
4. Persist remote-target observations without treating them as local authority.
5. Exercise interruption, authentication failure, concurrent publication, and
   server restart against an ephemeral repository daemon.

## Acceptance workflows

### Phase 1 — routine single-target dogfooding

This workflow must work without direct object IDs in ordinary commands:

1. Import a Git URL or initialize Wheat.
2. Materialize the selected view into a fresh workspace without exposing authority
   paths as source content.
3. Edit regular, executable, and symlink paths.
4. See accurate tracked-path status and diff without modifying repository state.
5. Create or revise a logical change with complete native actor/message metadata.
6. Revise an ancestor and observe descendant reapplication.
7. Inspect, materialize, and resolve a durable conflict.
8. Push a clean selected view to an ordinary Git remote and pass `git fsck`.
9. Reopen both repository and workspace and observe the same pinned state.
10. Consume versioned structured success and retryable stale-state errors from
    automation without scraping prose.

### Phase 2 — complete local workflow

Add workspace-wide status/capture with ignore rules, switching between two named
targets without overwriting local changes, configured lifecycle tooling, two
concurrent workspaces, target CAS failure, and dependency movement.

### Phase 3 — native client/server workflow

Run the Phase 1 and Phase 2 semantic corpus through `wheat-client` and the native
protocol, adding native synchronization, authentication failure, interruption,
and repository-daemon restart.

## Open questions for review

1. Does a workspace embed a repository store, or point to one?
2. Is there any persistent index/staging state, or only workspace differences
   selected into a logical revision?
3. Which target does a routine revision advance, and when does publication occur?
4. Are imported Git branch names immediately mutable native targets, or retained
   provenance until explicitly adopted?
5. Which ignore syntax is the native contract?
6. Which lifecycle tooling shape should Wheat standardize?
7. Can repository-declared executable tooling run without explicit local trust?
8. What machine-readable output and exit-code compatibility is promised before
   the native schema and protocol are locked?
9. When capture starts from a materialized view that is no longer the current
   working/published target, must it refuse, explicitly reapply onto the new view,
   or create a detached revision/application?
10. Does the current requirement that `add` find the change's application in the
    published view survive once working and published views can diverge?
11. Which advanced Git conveniences are genuinely required for the first Gothab
    dogfooding repository?

## References

- [`server-native-source-control-store.md`](./server-native-source-control-store.md)
  — canonical collaboration, storage, Git compatibility, and native protocol
  architecture.
- `crates/wheat-cli/` — current executable reference client.
- `crates/wheat-core/` — native model and deterministic application semantics.
- `crates/wheat-store/` — local immutable-object, packed-store, and publication
  implementations.
- `crates/wheat-git/` — Git import and projection compatibility boundary.
