# Wheat Local CLI — Design

Status: partial — the Phase 1 local workflow is implemented and accepted; Phase 2 labels/frontiers, workspace registry, ignore rules, `add --all`, and explicit revision collapse are landed; extended configuration keys are stored but not yet read; dependency/integration workflow, lifecycle tooling, and Phases 2–3 remain outstanding
Owner: bart
Last updated: 2026-09-26

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
  expose retained edit/application evidence and publish exact resolutions. Routine
  resolution infers the unique conflicted revision from its selected path; an
  explicit immutable revision remains available only for disambiguation.
- [x] **Git bootstrap and publication compatibility.** `wheat import` accepts a
  local Git repository or temporary-mirrors a Git URL into a fresh packed Wheat
  repository; `wheat push` projects a selected clean native view and publishes
  one Git branch with exact force-with-lease semantics.
- [x] **Linux workspace safety and materialization.** Explicit regular,
  executable, and symlink reads use descriptor-relative no-follow traversal;
  authority paths are reserved. Separate stores are linked from workspace-local
  metadata. Fresh workspaces materialize one pinned view, and existing workspaces
  move incrementally between pinned views using bounded Merkle diffs,
  exact-preimage transactions, crash recovery, and
  kernel-enforced no-replace/exchange operations. Changed or obstructed workspace
  paths fail closed without overwriting user data.
- [x] **Tracked workspace status foundation.** Human and versioned JSON status
  compare tracked paths with the pinned materialized tree, distinguish content,
  kind, and deletion changes, treat canonical-equivalent line endings as clean,
  and report repository-head advancement. Phase 2 status will distinguish unrelated
  repository movement from drift of the active label's binding.
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
- [x] **Linux-only platform contract.** The entire reference CLI fails before
  parsing or filesystem side effects on non-Linux platforms. macOS is Unix, but
  does not provide the Linux `renameat2` primitives on which crash-safe
  materialization depends; neither it nor Windows is currently supported. A
  non-Linux-only black-box test fixes the refusal contract for native CI.
- [x] **Structured automation contract.** Global `--format human|json` selects
  either the existing human presentation or one bounded, versioned terminal JSON
  document for success and expected failure. Stable categories, symbolic error
  codes, retryability, and exit classes have black-box coverage, including
  retryable stale-predecessor refusal without repository-head mutation.
- [x] **Phase 1 acceptance — routine single-target dogfooding.** One Linux-only
  black-box test runs the complete Git URL import through Git publication workflow
  across separate CLI processes. It proves exact regular/executable/symlink
  materialization and capture, read-only status/diff, complete structured authoring
  metadata, descendant reapplication, durable ID-free conflict inspection and
  resolution, clean selected-view projection with strict Git validation, pinned
  reopen, and the versioned retryable stale-state contract.
- [x] **Phase 2 model decision — immutable labels and workspace frontiers.** Bart
  settled short repository-local labels bound to immutable views, imported Git-ref
  provenance, per-workspace basis and selected frontier, and the zero/one/many
  selection rule. Label advancement and frontier consolidation remain deferred.
- [x] **Phase 2 — label and frontier workflow.** Short repository-local labels
  (list, create, rename, delete) are bound to immutable views via CAS-guarded
  envelope mutations. Switching activates a label as workspace basis and selects
  a retained descendant frontier by the zero/one/many rule. Refresh re-resolves
  the active label binding. Frontier inspection lists bounded descendant views.
  Deleting or renaming the workspace's active label is rejected.
- [x] **Phase 2 — imported-label provenance.** Every imported label retains its
  original Git ref without making the ref or its branch semantics native authority.
  Rename preserves provenance. The ref is exposed in structured label output.
- [x] **Phase 2 — ignore rules and workspace-wide status.** The `wheat-ignore`
  crate implements complete `.gitignore`-compatible pattern matching in
  `.wheatignore` files with hierarchical directory-scoped stacking, negation,
  directory-only patterns, `**` matching, and bracket expressions. `.gitignore`
  files are read as a fallback (with a per-scan diagnostic) when no sibling
  `.wheatignore` exists. `wheat status` now reports untracked and ignored paths
  via a bounded recursive workspace traversal that prunes ignored directories.
  Built-in rules always ignore `.wheat`, `.wheat-git`, and packed-store authority
  paths. User-global ignore lives at `$XDG_CONFIG_HOME/wheat/ignore`.
  Workspace-wide capture (Phase 2 of `wheat-local-tooling.md`) depends on this
  and remains outstanding.
- [x] **Phase 2 — extended configuration model.** The config schema is bumped to
  version 2 (backward-compatible with version 1). New keys: `core.editor`,
  `core.pager`, `core.default_label`, `core.default_format`. `deny_unknown_fields`
  is removed for forward compatibility. Hook and remote configuration sub-objects
  are designed but not yet implemented.
- [x] **Phase 2 — workspace registry and shared-store clones.** Strict bounded
  per-workspace pin documents under the separate store enumerate every workspace's
  basis, frontier, materialized state, selected change, and in-flight roots.
  `wheat clone <workspace-or-store> <destination>` creates a distinct workspace
  against the same store; repeated native clones reuse a uniquely matching local
  store by canonical remote identity. `wheat workspace list` exposes attachments,
  while `workspace unregister` removes only authenticated `.wheat` authority and
  preserves materialized/untracked files. Workspace metadata v4 carries a
  registration token; copied live workspace identities are refused.
- [x] **Open question — lifecycle tooling shape.** Settled: Git-style executable
  hooks with structured JSON context on stdin and optional structured JSON
  diagnostics on stdout. Timeout and output bounds enforced. Content-hash trust
  with re-consent on change. `wheat hook add/list/enable/disable/remove` commands.
  See `wheat-local-tooling.md` for the full design.
- [x] **Open question — replacement revision attribution.** Settled: the squashing
  actor is the replacement author and committer at squash time; the latest message
  and provenance remain, with deterministic, deduplicated `Co-authored-by` trailers
  for distinct superseded authors other than the squasher. Routine squash exposes no
  metadata overrides.
- [x] **Phase 2 — explicit revision collapse.** `wheat change squash [CHANGE]` and
  `wheat stack squash` collapse only unpublished checkpoint suffixes, preserve stable
  logical/edit identities and effective clean or conflicted semantics, remap labels
  and attached workspaces, and publish one completely validated non-monotonic state.
  Exact observed remote retained inventories form the immutable boundary; ordinary
  native publish remains exact and never squashes implicitly.
- [x] **Phase 2 — squash rewrite recovery atop workspace pins.** One bounded,
  store-scoped transaction pins the complete old/new repository and workspace sides.
  Store admission serializes every head-dependent command, recovers before another
  CAS can create a third head, and finalizes all affected workspace references without
  rewriting materialized bytes. Missing registrations are pruned; inaccessible,
  mismatched, or locked workspaces fail closed.
- [x] **Phase 2 — composed squash provenance.** (2026-09-26) Squash composes
  subsumption chains across the collapsed suffix: a boundary edit replaced
  through intermediate edits is recorded as replaced by the surviving final edits,
  and chains that end re-added or empty are dropped. A wholly unpublished change
  collapses to an initial revision with no evidence. Resolutions of intermediate
  checkpoints are dropped (the collapsed revision is re-proven at its spot); a
  resolution of a conflict still present at the boundary is kept with its
  replacement edits mapped through the same lineage. Anything else still fails
  closed. Extended beyond the original plan: retaining such a resolution also
  retains the view its application produced (a rewrite previously kept the
  application but dropped that view and failed verification), squash folds every
  suffix message instead of keeping only the latest, and `--no-coauthors` skips
  new trailers. See "Rewrite semantics".
- [x] **Git push commit messages come from the change.** (2026-09-26) See "Git
  compatibility workflow".
- [x] **Annotated labels and `wheat push --tag`.** (2026-09-26) Labels can carry a
  tagger, date and message; `push --tag` publishes them as create-only annotated Git
  tags whose object IDs are the same on every push, guarded by remote-branch
  reachability. Extended beyond the original plan: squash refuses to move an
  annotated label. See "Git compatibility workflow".

### Outstanding
- [ ] **Import keeps annotated tag data.** Import turns `refs/tags/X` into label
  `tags/X` but drops the tag object's tagger, date and message, so pushing an
  imported tag back needs the label re-created with `-m` and gives a different tag
  object. Store them as the label's annotation so imported tags round-trip.
- [~] **Phase 2 — dependency and integration workflow.** Expose stack revision,
  dependency movement, joins, deterministic descendant reapplication, and durable
  conflict outcomes as Wheat operations rather than Git-shaped rebase state.
  (2026-10-09: `wheat integrate` joins frontiers, combining revisions of one
  change that went their own ways.)
- [~] **Phase 2 — workspace-wide capture.** Extend `wheat add` with `--all` to
  capture all non-ignored modified and deleted tracked paths. Untracked files
  require `--include-untracked` or explicit path arguments. Depends on the
  completed ignore rules. (2026-09-25: `wheat add --all` landed with tests in
  `crates/wheat-cli/tests/workspace_add_all.rs`. `--include-untracked` does not
  exist yet; untracked files can only be added by naming them.)
- [ ] **Phase 2 — lifecycle tooling.** Choose and implement the contract for
  formatters, linters, policy checks, and notifications around capture, revision,
  materialization, and publication. The design is settled in
  `wheat-local-tooling.md`: Git-style executable hooks with structured JSON
  context on stdin, optional structured output, timeout/bounds, content-hash
  trust, and `wheat hook` management commands.
- [ ] **Phase 2 — repository discovery and local configuration.** Discovery walks
  ancestors to `.wheat` and is already implemented. Remaining: editor/pager
  integration, aliases, and compatibility remotes. (2026-09-25: `core.editor`,
  `core.pager`, `core.default_label`, and `core.default_format` can be set, but no
  command reads them yet. 2026-10-09: `wheat ignore import` exists.)
- [~] **Phase 3 — client/protocol boundary.** Introduce `wheat-client` and
  `wheat-protocol`, move local workspace operations behind the client contract,
  and remove the CLI's direct dependence on production store capabilities before
  adding remote mode. (2026-09-25: `wheat-protocol` exists and remote mode
  landed first. There is no `wheat-client` crate; the protocol engine lives in
  `crates/wheat-cli/src/remote.rs` and the CLI still depends on `wheat-store`
  directly.)
- [~] **Phase 3 — local synchronization.** Add persisted remote-view observations
  and incremental synchronization once the versioned native protocol exists;
  one-shot Git import remains a compatibility bootstrap, not native clone/pull.
  (2026-09-26: `remote.json` persists the last observed remote head, and
  `clone`/`fetch`/`publish` exist. Fetch is incremental: the server pushes only
  what the last fetched envelope lacks, and the client pulls any gaps. The local
  head counts its own generations; unpublished work is a local head envelope
  other than the last fetched or published one. 2026-10-09: a fetch keeps
  unpublished work, joining both states and merging labels three ways against
  the last fetch; `--force` drops it. `wheat integrate` then joins the
  frontiers, and `wheat pull` does both.)
- [ ] **Phase 3 — concurrent multi-workspace authoring policy.** Durable workspace
  identity, shared-store creation/list/detach, operation locks, and content pins are
  implemented. Still define user-facing integration when two workspaces advance
  independent frontiers and the remote synchronization policy around that state.
- [ ] **Defer — label advancement and frontier consolidation.** Do not define
  automatic label movement or combine retained descendant frontiers until an
  explicit later workflow and policy decision.
- [ ] **Open question — staging semantics.** Decide whether Wheat needs a
  persistent index-like state or whether explicit change/revision capture makes
  one unnecessary.
- [ ] **Defer — signing and verification.** Design native attestations only after
  actor identity and publication evidence are fixed; do not copy opaque Git
  signature headers into native identity.
- [ ] **Defer — advanced history utilities.** Blame, bisect, archive, grep, and
  rewriting already-published history follow the complete edit/materialize/integrate
  loop. Explicit collapse of unpublished revision checkpoints is specified above.
- [ ] **Release gate — freeze formats and start versioning.** Until Wheat is
  released, every on-disk and wire format (BinSchema schema, segments, catalog,
  content generations, SQLite locator) is edited in place: no `V2` structs, no
  version bumps, no decode fallbacks — existing stores are simply re-imported.
  At release this flips: from then on every format change must be versioned
  with an explicit migration or compatibility path. As part of the release,
  delete the agent memory
  `~/.claude/projects/-home-bart-Projects-gothab/memory/feedback_no_format_version_bumps.md`
  (and its `MEMORY.md` line) so agents stop editing released formats in place.

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
copied object IDs. The Phase 1 boundary includes a stable structured success/error contract;
workspace-wide untracked capture, immutable-label/frontier switching, and native
synchronization deliberately remain later phases.

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
  commit OIDs, and config must not become hidden native authority. Bart
  (2026-10-21): that "doesn't mean we cannot use them to see if git still
  considers something the same thing". A commit ID may tell Wheat that a Git
  commit is one it imported or exported, so it isn't imported again; what the
  Wheat state contains is still decided only by native records.

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

Where Git's terms fit the user intent—`status`, `diff`, `push`—using familiar
names is useful. Where they encode Git's storage accidents—mutable branches,
index, detached HEAD, rebase todo files, force-updating commits—the CLI should
expose Wheat concepts directly: logical changes, immutable revisions,
dependencies, applications, views, conflicts, labels, and frontiers.

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
revision, apply dependencies, bind a label, and select a frontier. Human-readable
output is a view over structured outcomes, not the only API contract.

## Workspace model

The original Phase 1 candidate was a workspace record carrying a selected mutable
target, materialized view, and expected repository head. That framing was useful
for proving pinned materialization and interruption safety, but it left Git-like
target movement and repository-global working/published views in the local model.

**Phase 2 follow-up (settled 2026-09-14):** a workspace instead records the active
label's immutable view as its `basis` and, when applicable, one explicitly
`selected_frontier`:

```text
Workspace {
    repository_identity
    active_label
    basis                 // exact immutable view observed from that label
    selected_frontier?    // retained descendant view; never a synthesized union
    observed_label_binding
    materialized_view
    selected_change?
    sparse_paths?         // deferred unless needed early
}
```

The active label name and its observed binding are client-local state; unrelated
repository-head movement does not by itself stale the workspace. The binding
supplies the workspace basis. Retained descendant views form the available
frontiers. Zero frontiers selects the basis itself; one existing frontier is
selected automatically; multiple frontiers require an explicit user choice.
Wheat never synthesizes or combines all frontiers implicitly. The
workspace's effective view is its selected frontier when present and otherwise
its basis. A newly created label defaults to that current selected/effective view.
Label advancement and consolidation of multiple frontiers are deferred rather
than smuggled into switching or capture.

The record does not claim that filesystem bytes still match the effective view;
status verifies that. Its update is atomic with completed materialization, or
recoverably journaled so an interrupted switch cannot masquerade as clean.
Repository-store placement remains open, but the semantic record itself is no
longer optional: repository state has no global workspace working or published
view from which this state could be reconstructed.

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
fresh workspace and incrementally moves an existing workspace between pinned
immutable views. A bounded iterative Merkle diff prunes equal subtrees, so planning
visits unequal directory nodes and mutation work is
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

A leaf that no longer holds its pinned value but already holds its exact target
kind and raw bytes, or a deleted leaf that is already absent, is unchanged:
`materialize` drops it from the journal instead of refusing it. Nothing is
overwritten, and only mismatched leaves pay for the second comparison.

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
  capture and materialization operation (one list, `WORKSPACE_CONTROL_NAMES` in
  `wheat-ignore`, shared by capture, materialization, and the built-in ignore
  rules);
- ask before writing any path with a `.git` component (any letter case). A tree
  may hold such files legitimately, but in a workspace that is also a git
  checkout they could plant hooks or config that git runs. Materialize, switch,
  refresh, restore, clone, and import list those paths before writing anything:
  a person at a terminal is asked, anything else (a script, `--format json`)
  is refused with `dot_git_paths` (exit 2) unless `--allow-dot-git` is given. A
  clone or import refused this way leaves the workspace set up but empty;
  `wheat materialize --allow-dot-git` finishes it;
- bound path count, individual content size, total bytes, and concurrent writes;
- journal per-path preparation/publication and define whether recovery rolls
  forward or rolls back after a partially visible multi-path update;
- fsync created/renamed/removed entries and each affected parent directory in the
  required order;
- update workspace metadata only after every planned mutation is durable.

Initial commands may be:

```text
wheat materialize [--source <view-or-label>] [DIRECTORY] [--allow-dot-git]
wheat restore [PATH ...] [--allow-dot-git]  # restore from the pinned workspace view
wheat switch <label> [--frontier <view>] [--allow-dot-git]  # label activation plus safe materialization
wheat integrate [--frontier <view> ...] [--allow-dot-git]  # join the active label's frontiers, then switch
wheat pull [--remote <url>] [--allow-dot-git]  # fetch, then integrate
```

`wheat pull` is `wheat fetch` followed by `wheat integrate`. With one frontier
left after the fetch there is nothing to join, and the workspace moves to it as
`wheat refresh` would. If the integrate step is refused, the fetch stays and the
error says to run `wheat integrate` once the cause is settled.

`wheat integrate` (2026-10-09) joins the active label's descendant frontiers, or
the chosen ones, into one view: all their changes composed in canonical order,
each at the revision it already has. Usually that is the label's view with what
the frontiers add composed onto it. When a frontier revised a change the label's
view already holds (`wheat resolve` on a conflict in it does), or a joined change
sorts below some of the label's changes, the joined view starts at the view below
the lowest such change on the label's path, and the label's changes above it are
composed again (`reapplied` in the JSON). A change that no longer applies cleanly
is recorded as a durable conflict, as in any composition. Every joined frontier
gets a lineage edge to the new view, so it is the label's one frontier, and the
workspace switches to it. Uncaptured tracked edits block it like a switch. If the
switch itself is refused (an untracked file in the way, or `.git` paths), the
joined view is kept and the error names the `wheat switch` that finishes it.

Frontiers may hold different revisions of one change, as when both sides of a
fetch revised it. A revision another one descends from is superseded by it.
Revisions that went their own ways from a common predecessor are combined by date
(Bart, 2026-10-09: "Order them by date and see if the changes in the revisions
still apply on top of one another"): the earliest by commit time, with each later
one's edit changes since the common predecessor laid on top, becomes a new
revision whose predecessor is the earliest and whose metadata is the latest's.
When one of them already holds the combined edits, it is used as it is. Like any
authored revision the combination must apply cleanly where it lands; if it does
not, integration is refused and names the change. The combination carries no
subsumption or resolution evidence from the revisions it combines.

Names are provisional; the safety, immutable-label, and explicit-frontier
semantics are not.

## Status and diff

`wheat status` should combine repository state with a workspace comparison. Its
human view should answer:

- which active label supplied the basis and which exact frontier/effective view is selected;
- whether zero, one, or multiple retained descendant frontiers exist;
- which tracked paths are modified, deleted, kind-changed, or conflicted;
- once Phase 2 ignore semantics land, which paths are untracked or ignored;
- which logical change/revision is the default capture target;
- whether repository authority changed since the workspace was materialized;
- which state is local-only versus observed from a configured remote.

Machine-readable output is versioned and opt-in through the global
`--format=json` option. Every invocation emits exactly one terminal document on
stdout and no prose diagnostics on stderr. Success nests the command-specific
result; failure carries a stable category, symbolic code, retryability, and a
bounded human diagnostic. Callers branch on those fields rather than parsing
prose. The default remains `--format=human`.

`wheat diff` needs three initial forms:

```text
wheat diff                         # tracked workspace vs pinned materialized tree
wheat diff --revision <id>         # revision vs predecessor/authored-against view
wheat diff --from <view> --to <view>
```

Exact retained view IDs and repository-local label selectors are resolved against
one authority snapshot. A label resolves to its immutable bound view; choosing
among multiple descendant frontiers remains explicit. Text files use canonical-LF
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
8. leave the workspace untouched unless a separate materialization is requested;
9. settle the pinned materialized tree for the captured paths, so they do not
   read as untracked or modified and `materialize` does not refuse them.

Settlement (`capture_settlement.rs`) never writes workspace files. It records in
the pinned tree each captured path whose raw workspace bytes are exactly the
resulting working view's value. If that accounts for every difference, the pin
moves to the resulting view and head. Otherwise the working view also holds
content this workspace has never materialized, typically a descendant reapplied
over the captured change. The pin then keeps its view and head, its tree gains
the settled paths, and `add` reports `materialize_required` with the message
"lineage changed …; run `wheat materialize` to update it". A captured path that a
reapplied descendant also edits is reported as `diverged` and pinned at the
captured change's own applied value, so the later materialization authenticates
the workspace bytes and installs the descendant's result over them. The settled
tree is rooted through the registry's pending-materialization slot before
metadata can reference it. An interrupted `add` recovers with the previous pin.

Workspace-wide capture must first have ignore semantics and configured bounds.
It must display or return the selected path set before mutation in interactive
flows. Untracked files are never included merely because they exist beneath the
workspace root.

## Revision checkpoints and explicit collapse

### Authoring model

A logical change has one stable `ChangeId`; each successful `add` creates an
immutable revision checkpoint of that same change. A successor revision is the
complete latest meaning of the change, not an imperative patch to apply after its
predecessor. Repeated authoring therefore deliberately retains a chain such as:

```text
base M → A1 / V1 → A2 / V2 → A3 / V3
```

These checkpoints are useful local recovery and provenance. They are not required
to express A3's final effect. Wheat retains them until the user explicitly rewrites
the draft history; storage maintenance must not decide that policy implicitly.

Canonical edits are the stable unit below a revision. An `EditId` hashes the path
and exact guarded or whole-file edit definition. It does not include the logical
change, predecessor revision, authored-against view, application target, or
repository generation. A squash or rebase therefore preserves unchanged
`EditId`s and their `ContentId`s while replacing graph-dependent evidence:
`RevisionId`, `ApplicationId`, `ConflictId`, `ViewId`, state, and envelope IDs may
change. Applying the same edit to a different authenticated view creates a new
application; it does not create a new edit definition.

### Commands and publication boundary

History collapse is explicit:

```text
wheat change squash [CHANGE]
wheat stack squash
```

`wheat change squash` collapses the unpublished checkpoint suffix of one logical
change. Descendant changes whose authenticated targets contain the replaced
revision are rebuilt in dependency order. `wheat stack squash` performs the same
normalization for every unpublished logical change in the selected stack, leaving
one unpublished revision/application/view boundary per logical change. It does not
flatten several logical changes into one change or one undifferentiated view.

Ordinary native `publish` transfers the exact local graph. It never squashes,
manufactures a compact remote-only projection, or makes local and remote object
identity diverge. With a wholly unpublished chain `A1 → A2 → A3`, collapse creates
one replacement revision `A'` with no predecessor. If A1 is already part of the
last observed remote authority, routine collapse rewrites only `A2 → A3` and keeps
A1 as `A'`'s predecessor. Rewriting a published boundary is a separate, explicitly
forced history-rewrite operation with remote authorization; it is not routine
squash behavior.

The unpublished boundary is resolved from the configured remote's exact
last-observed head, not from generation ordering alone. Every revision in that
remote state's retained-revision inventory is published and immutable to routine
squash, even when no current label selects it. With no configured remote, all
history after the workspace basis is local draft history. The operation pins the
repository head, remote observation, workspace selection, and expected latest
revision before planning, and publishes nothing if any becomes stale.

### Rewrite semantics

Squash reuses each change's latest complete canonical edit set rather than
recapturing a tree diff. It constructs a direct `EditTransition` from the retained
published predecessor, or an initial transition when no predecessor remains.
Edits unchanged across the collapsed suffix retain their IDs. Added and removed
edits remain explicit; subsumption and conflict-resolution evidence is composed
only where it remains semantically required at the retained boundary.

Rebuilding a descendant is conflict-preserving reconstruction, not a replay that
is permitted to discover unrelated new conflicts. The rewrite must preserve:

- every stable `ChangeId` and dependency relationship;
- each change's latest canonical edit set;
- the final effective tree, or conflicted partial tree;
- clean versus conflicted status;
- unresolved-conflict meaning: affected paths, participating edit identities,
  partial values, and failure/interference reasons.

Graph-dependent IDs may change. Sequential checkpoint conflicts at one location
may consolidate into one conflict because only the latest complete meaning is
retained. That is valid canonicalization when the final partial tree and unresolved
conflict semantics are equivalent. Any other semantic drift is an integrity
failure: abort before advancing repository authority rather than reporting that
squash introduced an ordinary conflict.

The replacement revision is a new authored object. It preserves logical change
metadata and the latest complete edit set, records the retained predecessor
boundary, and receives honest revision metadata rather than impersonating an old
checkpoint. The actor running squash becomes both author and committer at squash
time. The latest checkpoint message and provenance remain; one canonical
`Co-authored-by: Name <email>` trailer is appended for each distinct superseded
author other than the squasher, deduplicated against existing equivalent trailers
and sorted deterministically. Routine squash has no metadata override flags.

Follow-up (2026-09-26): keeping only the latest checkpoint message lost the change's
title. `wheat change new -m` stores its text in the first revision, and later
`wheat add` checkpoints have empty messages, so a squashed change often ended with an
empty message. Squash now folds every suffix message, oldest first: empty and exactly
repeated messages are skipped, and existing `Co-authored-by` trailers are gathered into
one block at the end before new ones are appended. It also gained one metadata flag,
`--no-coauthors` on `wheat change squash` and `wheat stack squash`. The flag skips the
new trailers, for the case where the superseded "authors" are the same person under an
old identity; trailers already in the messages stay.

Follow-up (2026-09-26), evidence composition as built: the suffix is walked oldest
first. For each boundary edit, the pass tracks the set of edits that currently replace
it. A subsumption `s → R` substitutes `R` for `s` in every tracked set. A removal
without subsumption drops the edit. At the end, `s → R` is emitted only when `s` is
still removed and `R` is non-empty; `R` must lie within the collapsed revision's
added edits, or squash fails closed. A resolution is kept only when the conflict
it resolves belongs to a revision outside the suffix; its replacement edits are
mapped through the same lineage and must be selected. Resolutions of intermediate
checkpoints are dropped, since the collapsed revision is re-proven at its spot. With
no retained boundary, nothing is emitted. When a kept resolution's application is
newly retained, the view that application produced is retained with it.

### Roots, transactions, and reporting

A rewrite changes repository semantics and uses a completely validated,
non-monotonic maintenance transition. All replacement immutable objects are made
durable before one head CAS. Losing or interrupted planning may leave unreachable
immutable objects, never a partially published graph.

A store must be able to enumerate every attached workspace's effective-view pin
before destructive history collapse is enabled. The invoking workspace is not the
only possible root: old history remains retained while another workspace, label,
base view, published boundary, snapshot, or other declared owner still selects it.
Repository-head publication and affected workspace selections use one store-scoped
recoverable transaction; a crash resolves to the complete old or complete new
selection, never an untracked dangling workspace. Every authenticated workspace
whose basis, observed label binding, selected frontier, or materialized provenance
maps exactly to an equivalent replacement view moves in that transaction. Its
materialized bytes and tree do not change. An affected inaccessible,
identity-mismatched, or locked workspace blocks squash. A registry entry whose
recorded workspace path is genuinely absent is a broken attachment and is removed
during preflight rather than retaining history indefinitely.

Human and structured output report the replaced and replacement revisions/views,
reapplied descendant changes, consolidated conflicts, retained roots that prevented
removal, and whether old objects merely became GC candidates. Squash does not
physically delete bytes. That is the separate destructive `wheat gc` operation;
physical representation changes belong to nondestructive `wheat repack`.

`wheat repack [--force]` is implemented for existing local packed/hybrid stores. It
holds store-wide mutation admission while a read-only plan authenticates the exact
repository and physical authorities and estimates temporary capacity. Insufficient or
unmeasurable capacity asks for confirmation only on an interactive terminal;
non-interactive use must pass `--force`, which bypasses only this capacity gate. After
admission, repack streams every authenticated structural record into bounded replacement
segments and writes one authenticated content-generation pack. Every maximal retained
file lineage tip is full, historical content reverse-deltas toward a deterministic newer
version with depth at most 16, and ambiguous lineage falls back to full content. One V2
physical-catalog head commit selects both representations; interruption recovers to all
old or all new, and old local files retire immediately under the process-exclusive reader
lock. Repository head bytes/generation/envelope, logical IDs, retained and unrelated
physical inventory, workspace metadata, and materialized files remain unchanged. A
canonical second invocation is a true no-op. Wholly loose stores and remote/shared
backends are refused; loose migration and server-authoritative maintenance remain
separate work.

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
or diagnostic output. Editor, pager, default-label, and output-format settings
remain part of the broader configuration work in Phase 2.

## Active workspace changes

Phase 1 fixed an `authoring_basis` to the first materialized view and derived an
active change delta against a repository-global working view. That proved the
basis-relative listing behavior, especially for Git imports, but the global
working/published distinction is not part of the settled Phase 2 model.

**Phase 2 follow-up:** the active label supplies the workspace `basis`; the
workspace stores a selected retained descendant frontier when one is available.
The selected frontier, or the basis when none exists, is the effective view.
`wheat changes` reports the effective-view delta from the basis in deterministic
dependency order and marks the selected logical change. Explicit `add --change A`
is a one-operation override and does not replace selected change B.

Captured changes are repository graph state, not disposable workspace state. They
remain retained when another label is activated and reappear when switching back
to a basis/frontier that reaches them. By contrast, uncaptured filesystem edits
are not graph state and block switching; Wheat must not overwrite, hide, carry, or
implicitly capture them. Landed/closed/withdrawn lifecycle remains undefined until
a later label-advancement policy exists.

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

## Repository labels and integration workflow

The earlier candidate treated a branch-like name as a mutable publication target
and asked whether capture advanced that target, a separate working view, or both.
That reasoning exposed the ambiguity, but Phase 2 deliberately does not adopt the
candidate.

**Phase 2 follow-up (settled 2026-09-14):** labels are short names scoped to one
repository and bound to immutable views. They are navigation and provenance
handles, not change identity and not implicitly advancing branches. Imported
labels retain the exact original Git ref as provenance while exposing a short
repository-local name; the provenance does not make that ref native authority.
Creating a label without an explicit view binds it to the workspace's current
selected/effective view.

Activating a label installs its bound view as workspace basis and considers only
retained descendant views as frontiers. With no descendant frontier, the basis is
effective. With exactly one existing frontier, Wheat auto-selects it. With more
than one, switching requires an explicit frontier; it never synthesizes a union or
silently composes every tip. Captured graph changes survive switching and reappear
when reached again, while any uncaptured filesystem edit blocks the switch.

Label advancement and consolidation of multiple frontiers are explicit deferred
work. When designed, stack editing and integration should expose dependency
changes and application outcomes directly. Revising the bottom of `A -> B -> C`
should show new applications of descendants without pretending that old revisions
ceased to exist. Combining independent tips requires an explicit integration/join
operation with durable conflicts, not an interrupted merge directory or an
implicit "all frontiers" result.

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
- before future label advancement or frontier consolidation;
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

Push writes one Git commit per change application. Its author and committer come
from the change's newest revision. Its message (2026-09-26) comes from the change,
not from that revision alone. Previously an empty newest message became "Wheat
revision". A revision with Git provenance (imported history) keeps its message byte
for byte, so round trips stay exact. Otherwise the message is:

- the change title, then its description;
- then every non-empty revision message along the predecessor chain, oldest first,
  with a leading line equal to the title dropped;
- empty and exactly repeated parts are skipped;
- `Co-authored-by` trailers are gathered into one block at the end.

Composition is bounded by `ExportLimits::max_note_revisions` (checkpoint reads per
export) and `max_message_bytes`. Exceeding either is a typed limit error, never a
truncated message. A change missing from the state's change map is refused. The
`git_export_notes` quick bench case tracks the cost.

Tags (2026-09-26) come from annotated labels. `wheat label create tags/v1 -m
<message> [--view V] [--tagged-at T]` stores the tagger (the configured actor), the
date and the message on the label itself, in the label manifest. Rename keeps them,
and squash refuses to move an annotated label to a new view
(`moves_annotated_label`). `wheat push <git-url> --tag v1` then publishes label
`tags/v1` as an annotated tag at `refs/tags/v1`. The tag object's ID must never
change, so:

- the commit is exported by one fixed rule: anchor on the newest imported commit
  in the view's chain (so imported history keeps its original IDs), else export
  fresh. Native commits read no clock, environment or Git config, so a fresh
  projection, another machine or another remote gives the same commit;
- the tag object holds only the commit, name, stored tagger, date and message (the
  message gets a trailing newline), with no signature;
- the remote's branch tips are fetched into `refs/wheat/remote-heads/` (at most
  1,024 branches, else `too_many_remote_branches`). The tag is refused
  (`tag_commit_not_on_remote_branch`) unless its commit is reachable from one of
  them, so a tag can't point at a commit no branch has;
- the push is create-only. The same tag object already on the remote is "up to
  date"; a different one is refused (`tag_exists`). There is no force option.

A tag push costs three Git processes (`ls-remote`, one fetch, one
`for-each-ref --contains`) whatever the branch count. Import still turns a Git tag
into a plain label and drops the tag object's tagger and message (see the
Outstanding list).

A configured compatibility remote may remove repetitive URL/ref arguments later,
but configuration must identify it as a Git projection endpoint. Receiving
changes from it remains an explicit import/reconciliation operation; a changed
Git ref must not silently redefine canonical Wheat state.

Pulling from Git (2026-10-21; acceptance test
`crates/wheat-cli/tests/git_round_trip.rs`). Bart chose `wheat pull <git-url>`:
the explicit reconciliation above. It imports only the commits the store doesn't
know yet, walking back from the remote tip to a known commit: one imported earlier
(its recorded Git provenance) or one a push recorded (see below), so Wheat's own
pushed changes come back as the same changes. Unpushed Wheat
changes are then reapplied on top of the new Git commits, as `git pull --rebase`
would, so the next push fast-forwards and leaves the other commits exactly as
they were pushed. Bart's further choices:

- the branch is the active label's Git ref from the import, overridable with
  `--from <ref>`; a native label without one needs `--from`;
- the label stays where it was imported; the workspace selects the new frontier,
  as native pull does. Pull requires the label to have a single frontier;
- merges are imported first parent only: one change per first-parent commit, with
  edits against the first parent, so the merge commit itself round-trips;
- pushed commits are recognised through the repository state's projected commits
  (2026-10-21, replacing reverse refs in the projection). Bart chose to record them
  in the Wheat store, in the state, as "application A projects as commit C": a set
  of 52-byte members, the 20-byte Git commit then the application ID, so the
  applications of one commit are a range read. Push records each native commit it
  writes before Git has it; export is deterministic, so a failed push records
  nothing false. Every member's application must be retained (full and transition
  validation check it), a union joins both sides' members, a squash drops the
  members of the applications it drops, and removing a member is non-monotonic.
  The record travels with the state over `wheat://`, so losing the disposable
  projection, or pulling in another store, neither duplicates pushed changes nor
  rewrites them on the next push. The projection keeps no reverse refs and nothing
  re-hashes trees. Only SHA-1 Git commits are recorded; SHA-256 repositories are
  not supported. Recording advances the repository head, so `wheat status` reports
  the repository advanced after a push. An export patches each tree it writes from a
  seed: first the nearest projected commit at or below the anchor, then the commit
  it just wrote. Shared subtrees and files keep the seed's Git entries, so a push
  reads only the paths changed since that commit. Imported commits never seed: import
  normalises CRLF and modes, so their trees aren't what an export writes;
- a reapplied change keeps its ID and gets a new revision depending only on the new
  tip, with the author kept and the puller as committer (like `git rebase`). It is
  authored at its old position, so it stays valid, and a reapplication that no
  longer fits is recorded as a durable conflict.

Pull refuses, without publishing anything, a dirty workspace, a label with several
frontiers, a branch the remote lacks, a branch sharing no first-parent commit with
the workspace, more than 10,000 new commits, and a recognised commit the workspace
holds elsewhere than on its own path (a pushed change revised since, for one): Git
IDs only recognise commits, and which changes the repository holds stays its own
records' to decide. A `wheat://` or `wheat+tls://` URL is a native pull.

## Repository discovery

Commands should discover a workspace/repository by walking ancestors from the
current directory, stopping at filesystem and explicit safety boundaries. An
explicit `--repository` always wins. Discovery must distinguish a repository
store from an editable workspace so a store nested near source files is not
mistaken for permission to modify them.

## Errors and automation

The JSON terminal envelope is `wheat-command-result` version 1. It contains
`ok`, the resolved command name, and either `result` or `error`. Error objects
contain `category`, `code`, `retryable`, and `message`. Exit classes are stable:
0 success; 2 invalid invocation/input; 3 retryable stale state; 4 unresolved
conflict; 5 configured limit; 6 integrity failure; 7 unsupported platform; 8
I/O or transport failure; and 1 unexpected internal failure. The symbolic error
code remains the finer-grained automation key.

`cat` is bounded by the same terminal-output budget: human mode writes raw bytes,
while JSON mode represents them as a structured byte array until the native
client/protocol introduces a streaming binary response. Help/version are terminal
success documents in JSON mode; parser failures are terminal error documents.
A downstream stdout write failure may necessarily leave an OS-level partial write;
JSON mode never follows such a failure with prose on stderr.

Routine failures need distinct structured categories:

- workspace dirty or blocked path;
- repository head changed;
- predecessor revision stale;
- label/frontier selection became stale or is ambiguous;
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
   Complete: one global human/JSON switch covers parser, read, mutation, raw-content,
   and failure outcomes; diagnostics, exit classes, and retryable stale predecessor
   behavior are qualified at the binary boundary.
8. Run a black-box Git-import → materialize → edit → diff → revise → push flow,
   including machine-readable retryable failures. Complete: the unified Linux-only
   acceptance test exercises the sequence across separate processes, including
   ID-free durable conflict resolution, strict Git validation, exact pinned reopen,
   and a stale refusal that leaves the repository head unchanged.

### Phase 2 — labels, frontiers, and repository tooling

1. Implement short repository-local labels bound to immutable views, preserving
   original Git ref provenance for imported labels.
2. Implement workspace basis/frontier switching: zero uses basis, one existing
   frontier auto-selects, and multiple require an explicit choice; never synthesize
   an all-frontier view. Uncaptured filesystem edits block every switch.
3. Add workspace-wide status/capture with complete ignore semantics.
4. Add dependency movement and explicit integration commands with durable conflict
   output; label advancement and frontier consolidation remain deferred.
5. Add repository discovery and typed local configuration.
6. Choose and implement lifecycle tooling/hooks.
7. Qualify concurrent workspaces and stale-head outcomes locally.

### Phase 3 — native synchronization

1. Introduce `wheat-client` and `wheat-protocol`, extract workspace operations
   behind the client interface, and stop the CLI from depending directly on the
   production store implementation.
2. Run the same semantic operations through the versioned native protocol.
3. Add native clone/fetch/publish with resumable bounded object transfer.
4. Persist remote-view observations without treating them as local authority.
5. Exercise interruption, authentication failure, concurrent publication, and
   server restart against an ephemeral repository daemon.

## Acceptance workflows

### Phase 1 — routine single-target dogfooding

This workflow works without direct object IDs in ordinary commands. The executable
acceptance contract is
`crates/wheat-cli/tests/phase1_acceptance.rs::phase_one_routine_single_target_workflow_across_processes`:

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

Add workspace-wide status/capture with ignore rules; switch between immutable
labels using basis plus the zero/one/many frontier rule; prove that captured graph
state reappears when switching back and uncaptured filesystem edits block a
switch; preserve imported Git-ref provenance; and exercise configured lifecycle
tooling, two concurrent workspaces, stale selection, and dependency movement.
Label advancement and multi-frontier consolidation are not acceptance requirements.

### Phase 3 — native client/server workflow

Run the Phase 1 and Phase 2 semantic corpus through `wheat-client` and the native
protocol, adding native synchronization, authentication failure, interruption,
and repository-daemon restart.

## Open questions for review

1. Does a workspace embed a repository store, or point to one?
2. Is there any persistent index/staging state, or only workspace differences
   selected into a logical revision?
3. Which ignore syntax is the native contract?
4. Which lifecycle tooling shape should Wheat standardize?
5. Can repository-declared executable tooling run without explicit local trust?
6. What machine-readable output and exit-code compatibility is promised before
   the native schema and protocol are locked?
7. What explicit operation eventually advances a label, and what review/policy
   evidence does it require?
8. What explicit operation consolidates multiple frontiers, and how does it retain
   each source frontier and conflict outcome?
9. Which advanced Git conveniences are genuinely required for the first Gothab
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
