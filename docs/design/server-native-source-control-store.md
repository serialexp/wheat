# Wheat — Server-Native Source Control

Status: partial — the native guarded-edit model, persisted copy-on-write mutation, incremental authoring, retention/reopen, Phase 1 local CLI workflow, Git import paths, local active-pack growth/sealing, and Phase 2 immutable labels and workspace frontiers are implemented and qualified; a first native protocol and server exist; model/store qualification, concurrency, deletion/GC, forge review policy, protocol resumability and remote-CLI qualification, and product commitment remain open
Owner: bart
Last updated: 2026-09-25

## Implementation status

Tracking the gap between the existing Git-backed forge and a possible
server-native source-control model. Checked items are capabilities that exist on
`main`; they are prerequisites and evidence, not an implementation of the model
described here.

### Done

- [x] **Custom client/server seam.** `git-remote-gothab` owns the
  `gothab://` and `gothab+ssh://` entry points through Git's remote-helper
  protocol, mediating `list`, `fetch`, and `push` rather than exposing only a
  conventional Git remote URL.
- [x] **Storage placement outside the repository ODB.** Gothab can transfer
  large blobs out of band, route blob bytes through shared content-addressed
  storage, and retain a structural repository ODB as a compatibility and
  serving representation.
- [x] **Server-side repository authority.** `gothab-gitd` owns repository
  placement, mutation admission, publication fencing, push journaling,
  ownership publication, recovery, snapshots, and the Git data plane.
- [x] **Forge concepts richer than Git.** Pull-request revisions, durable
  stack identity, review relocation, merge gates, CI observations,
  attestations, and publication receipts already preserve server-side state
  that cannot be represented faithfully by refs and commit OIDs alone.
- [x] **Bounded walking skeleton — pure core.** `wheat-core` defines native
  stable change IDs, immutable revisions, dependency graphs, deterministic
  views, applications, structured durable conflicts, and compare-and-swap
  publication with no Git or gix dependency.
- [x] **Bounded walking skeleton — executable workflow corpus.** Focused tests
  cover `A -> B -> C` descendant reapplication, independent roots, sibling
  branches, multi-parent joins, topological ordering, persistent conflict and
  scoped resolution, application to unrelated target views, rollback after
  invalid creation, stable identities, and stale revision/publication rejection.
- [x] **Bounded walking skeleton — local CLI shell.** The standalone `wheat`
  binary initializes a prototype repository, persists its model locally, and
  exposes structurally parsed status, log, change creation/revision, conflict
  inspection, and resolution commands.
- [x] **Bounded walking skeleton — retained content bytes.** `wheat-store`
  retains content by native `ContentId` outside `wheat-core`, verifies bytes on
  read, deduplicates identical content, distinguishes missing from corrupt
  objects, and lets the CLI retrieve retained content after reopening.
- [x] **Bounded walking skeleton — self-describing views.** View identity now
  covers immutable application identities, unresolved conflict identities, and
  required visible content as well as the tree and revision selection. Core
  queries enumerate conflicts by view, change, and path, and a pure policy
  predicate evaluates whether a view has zero unresolved conflicts.
- [x] **Phase 1 — complete the reference CLI and local workflow.** The Linux-only
  reference CLI has explicit selected-path capture, persisted composition and mutation,
  retained-view publication, durable ID-free conflict inspection/resolution, complete
  metadata and validation, loose/packed reopen, and bounded versioned automation output.
  A unified black-box acceptance test qualifies Git URL import, exact workspace kinds,
  read-only status/diff, descendant reapplication, conflict materialization/resolution,
  clean Git projection plus strict validation, pinned reopen, and retryable stale refusal.
  Workspace-wide capture, immutable-label/frontier switching, and remote mode belong to
  later phases rather than this Phase 1 contract.
- [x] **Phase 2 model decision — immutable labels and workspace frontiers.** Bart
  settled short repository-local labels bound to immutable views, imported Git-ref
  provenance, per-workspace basis and selected frontier, and the zero/one/many
  selection rule. Label advancement and frontier consolidation remain deferred.
- [x] **Phase 2 — immutable labels and workspace frontiers.** Implement short
  repository-local labels bound to immutable views, retain original Git ref
  provenance on imported labels, and store workspace basis plus selected frontier.
  Zero descendant frontiers selects the basis, one existing frontier auto-selects,
  and multiple require an explicit choice; never synthesize their union.
  (`crates/wheat-cli/src/label_service.rs`, `frontier_service.rs`,
  `BranchRecord::original_git_ref`, `WorkspaceMetadata::{basis,
  selected_frontier}`; tests in `crates/wheat-cli/tests/labels.rs` and
  `materialization_recovery.rs`.)
- [x] **Guarded edits — canonical model and retention.** Standalone content-addressed
  guarded/exact edit definitions, revision selections and transition lineage,
  per-edit application evidence, historical closure, semantic replay, and reopen
  verification are implemented in the Wheat prototype.
- [x] **Guarded edits — deterministic authoring and CLI.** Symmetric unique-context
  capture plus two lines per side, explicit wider guards, component-level
  consolidation/lineage, `wheat add`, and inspectable edit definitions are implemented.
- [x] **Guarded edits — application and conflict corpus.** Exact simultaneous guard
  matching, typed per-edit outcomes, durable ambiguity/interference conflicts,
  bounded work/output, and rollback are covered by the core/store/CLI corpus.
- [x] **Persisted daily mutation path.** Local loose and atomic packed repositories support
  root-backed `change new`, `add`, and conflict resolution without hydrating retained history.
  Composition preserves the longest unchanged authenticated prefix, loads edit bodies only for
  the affected suffix, batches metadata before head CAS, and preserves imported-ref
  manifests. A fresh 1,607-change Gothab import qualified frontier creation in 12.91
  seconds, revision in 3.75
  seconds, reopen/status in 3.19 seconds, and explicit complete validation in 757.35 seconds;
  the retained-history audit remains deliberately distinct from interactive mutation latency.
- [x] **Decision gate — guarded-edit edge algorithms and encoding.** The prototype V1
  now fixes symmetric context growth, anchored boundaries, deterministic bounded line
  mapping and fixed-point consolidation, content-derived identity and transition
  lineage, canonical LF text versus byte-exact binary/symlinks, exact file operations,
  resolution evidence, and bounded failures. The native schema remains experimental
  until the separate schema-lock decision gate below is approved.
- [x] **Phase 2 — active pack growth and sealing.** Durable local mutations now use one
  process-exclusive active pack, preserve payload offsets, replace its cumulative authenticated
  tail, journal exact rollback state, publish a checksummed physical head, update locator rows
  incrementally, and seal/start a new pack at a 128 MiB initial policy bound with independent
  count/payload/tail limits. Resumable imports acknowledge bounded groups only after one durable
  flush and authenticated readback. Isolated atomic imports use a one-pass deferred writer that
  appends each payload once and emits a final V1 tail only on rollover or consuming close before
  the terminal filesystem sync and no-replace rename. A fresh 1,624-change Gothab import produced
  seven packs (six near 128 MiB) in 290.61 seconds with 317 MiB peak RSS; reopen status took 1.18
  seconds, log 0.41 seconds, and explicit complete validation 178.18 seconds. Crash, tampering,
  rollover, close-failure, restart, acknowledgement, cached-read, and orphan-publication cleanup
  regressions are covered. Marker-authenticated stale atomic-import directories are reclaimed under
  the per-destination lock; unrelated, malformed, and symlinked candidates are preserved. An ignored
  release benchmark tracks the intentionally expensive one-object durable path: 128–2,048 small
  appends sustained roughly 39–49 objects/second on the qualification host, reinforcing that bulk
  ingestion must use bounded batches or the deferred writer rather than treating per-object fsync as
  a throughput path.

### Outstanding

- [~] **Guarded edits — interdiff and review continuity.** Revision transitions expose
  preserved, added, removed, subsumed, and resolution relationships, and applications
  bind outcomes to exact edit definitions and targets. Forge reviewer evidence and
  approval/publication policy remain a separate follow-up.
- [ ] **Separate follow-up — structural review warnings.** Use language-aware
  enclosing-scope analysis to flag broader context changes without weakening exact
  guards or making parser behavior part of native identity/application validity.
- [ ] **Decision gate — strategic commitment and allocation.** Decide whether
  a server-native model/store becomes a parallel product-engineering effort,
  and what ongoing capacity it receives relative to completing the Git-backed
  forge.
- [ ] **Phase 0 — Git-assumption inventory.** Catalogue every place where a
  ref name, commit OID, tree, push, or Git repository is treated as canonical
  identity rather than as transport or projection.
- [ ] **Phase 0 — workflow requirements.** Derive the logical model from
  concrete server workflows: publication, revision, review, CI, conflicts,
  concurrent agents, authorization, audit, retention, replication, deletion,
  and recovery.
- [ ] **Decision gate — canonical primitives.** Decide the meanings and
  identities of change, revision, dependency, conflict, operation,
  publication, workspace, and Git projection before choosing an on-disk or
  wire encoding.
- [ ] **Decision gate — lock the native schema.** Until this box is checked,
  every Wheat schema version is experimental: change `V1` types and tags in
  place, regenerate code and fixtures, and discard prototype repositories
  rather than carrying migrations or compatibility code. Checking this box
  declares existing encodings persistent compatibility contracts; from that
  point onward, incompatible changes receive new version tags and the evolution
  rules below apply.
- [ ] **Decision gate — compatibility contract.** Decide what must round-trip
  through ordinary Git, what may be a lossy projection, and whether
  `git-remote-gothab` initially delegates to Git or materializes Git objects
  from a native service.
- [~] **Phase 1 — qualify the native model and store prototype.** Generate and
  integrate the new BinSchema canonical-object encoding; replace the walking
  skeleton's deliberately simple whole-file transformations and YAML state
  snapshot with complete content manifests and reachability, transactional
  metadata/content publication, dependency validation, and property tests. The
  prototype now retains verified content bytes, but does not yet prove ownership,
  recovery, or safe reclamation; Git and gix remain outside the core.
  (2026-09-25: BinSchema encoding, YAML-snapshot removal, content manifests,
  transactional publication, proptests in `wheat-core/tests/generated_contracts.rs`
  and `wheat-store/tests/generated_backend_parity.rs`, and ownership proofs in
  `wheat-store/src/ownership.rs` have landed. Safe reclamation remains.)
- [~] **Phase 1 — complete the conflict and concurrency corpus.** Extend the
  current chain/branch/join/conflict/CAS tests to concurrent human/agent
  mutations, independent simultaneous conflicts, retries, dependency movement,
  and property-tested deterministic outcomes without a mutable Git ref as truth.
  (2026-09-25: `wheat-store/tests/actor_concurrency_corpus.rs` covers concurrent
  human/agent CAS, retry/reapply, competing resolutions, and dependency movement.
  Independent simultaneous conflicts and property-tested concurrent outcomes
  remain.)
- [~] **Phase 1 — lifecycle and content-retention prototype.** Exercise
  publication, crash recovery, content manifests, shared-content retention,
  snapshot/restore, and deletion as native store invariants rather than adding
  them after the format is fixed. (2026-09-25: everything except deletion has
  landed, including `wheat-store/src/snapshot.rs` and
  `tests/snapshot_restore.rs`. Nothing deletes unreachable data yet.)
- [ ] **Defer — label advancement and frontier consolidation.** Define explicit
  operations and policy for moving a label or consolidating several retained
  descendant frontiers. Repository state has no global workspace working/published
  view and routine mutation must not introduce one implicitly.
- [ ] **Phase 2 — immutable-object and publication-store interfaces.** Keep
  content-addressed reads/writes independent from the tiny mutable repository
  head. Portable async traits, local loose-object/locked-head implementations,
  and an Apache `object_store` adapter qualified against in-memory storage and
  SeaweedFS now exist; Gothab/ObjectRouter, PostgreSQL, and journal/checkpoint
  adapters remain to be built according to their actual durability and fencing
  guarantees.
- [~] **Phase 2 — persistent Merkle collections.** Canonical directory nodes,
  validated typed decoding, copy-on-write path edits, structural sharing, and
  loose/generic-object-store reopen coverage now exist. Revision/application maps,
  conflict/content/retained sets, root-backed persisted views/manifests/state,
  shared identity/closure builders, and verified root-only publication/readback
  are implemented. A resumable disk-journaled Git importer now stages canonical
  commits without executable hydration and retains imported refs with their provenance.
  Interactive executable models remain hydrated, while CLI reads and daily mutations operate
  directly over persisted roots with copy-on-write prefix reuse. Change-ID indexing, compressed
  radix qualification and wide-directory sharding remain.
- [~] **Phase 2 — indexed sealed segments and ranged reads.** The local backend now
  packs canonical metadata and small content into immutable BinSchema-defined,
  footer-indexed segments with paged sorted indexes, authenticated directories,
  an additive crash-safe catalog, and a persistent derived SQLite locator tied to atomically
  published catalog and active-pack heads. Ordinary reopen reuses the checkpoint-matched locator;
  sealed sources authenticate lazily once per process and retain their decoded indexes, while
  active reads use the already-authenticated in-memory index. Legacy, missing, stale, or corrupt
  locator state is rebuilt from immutable authority. Large content remains standalone. Local
  reopen/corruption/interrupted-install tests exist; remote range reads, coalescing, compaction,
  GC, and broader remote-store failure injection remain unqualified.
- [ ] **Phase 2 — conservative loose-object collection.** Never collect an
  unreachable loose object less than 24 hours after its backend modification
  time; verify the proposed closure immediately before publication and initially
  leave sealed-segment deletion disabled.
- [~] **Phase 2 — physical store qualification.** Benchmark and refine object
  encoding, index fan-out, active-pack sealing thresholds, deltas, sharding, replication,
  caching, and garbage collection against deep stacks and wide agent fan-out on local files and
  real object-storage range requests. (2026-09-25: deltas and sharding are
  implemented, with benchmarks such as `object_store_bench`, `packed_active_bench`,
  `content_repack_scale`, and `s3_packed_test::publish_clone_s3_benchmark`.
  Replication, GC, deep-stack and fan-out benchmarks, and real ranged remote
  segment reads remain; the remote cache downloads whole segments.)
- [~] **Phase 2 — native protocol and remote CLI.** Specify a resumable,
  versioned client/server protocol and run the same workflow corpus through the
  reference CLI against an ephemeral repository daemon. (2026-09-25: the
  versioned protocol (`wheat-protocol`), `wheat-server`, and CLI
  `clone`/`fetch`/`publish` exist. Transfers are not resumable, and no test runs
  the `wheat` binary against a `wheat://` server; the server end-to-end tests use
  hand-written protocol clients.)
- [~] **Phase 3 — separate gix compatibility adapter.** Import Git into native
  objects and project selected native views to disposable Git repositories or
  synthesized packs without allowing Git identities to become authoritative.
  (2026-09-25: `crates/wheat-git` imports, projects, and pushes, and neither
  `wheat-core` nor `wheat-store` depends on gix. Projection handles only clean,
  linear history, and synthesized-pack serving does not exist.)
- [ ] **Phase 3 — repository-daemon integration.** Let gitd (or its successor)
  serve native repositories through the native protocol and Git projections
  through the gix adapter, with one explicitly canonical store per repository.
- [ ] **Open question — migration.** Determine whether existing repositories
  are imported once, dual-written, lazily promoted, or remain permanently
  Git-canonical until an explicit conversion.
- [ ] **Open question — product boundary.** Decide whether the store is an
  internal substrate, a separately consumable service, an open protocol, or
  some combination; each changes compatibility and competitive incentives.

## Why this exists

Gothab began with a deliberate sequencing assumption: build a useful hosted
forge on Git first, learn where Git's model is insufficient from real server
work, and repair or replace the underlying model afterward. Continued work on
repository storage, review, stacks, conflict handling, ownership, and
publication has made the limitation increasingly concrete. Gothab often has to
infer stable collaboration concepts from mutable refs and content-derived
commit identities, then preserve those concepts in surrounding databases and
journals.

That sequencing remains capable of producing valuable requirements, but it now
has a time-sensitive downside. East River Source Control (ERSC) publicly says
it is taking the opposite route: build a new storage and collaboration
substrate, based on a Jujutsu-style model and compatible with Git, then build a
code-hosting and review product above it. Its private implementation depth is
unknown and must not be inferred from its sparse public product surface. The
relevant competitive fact is the direction of travel, not any particular
person on its team.

The two approaches may converge on the same market from opposite directions:

```text
Gothab: complete Git forge -> richer server model -> native store
ERSC:   native store/model -> collaboration and review -> complete forge
```

If both products eventually sell the same agent-oriented development story, a
venture-funded company can purchase more parallel agent execution, more human
supervision, more specialist workstreams, and more experiments. Agentic
software development does not erase capital advantage; it can amplify it where
work decomposes across storage, review, operations, UI, compatibility, and
enterprise features. Gothab should not base its strategy on ERSC lacking
unannounced forge work, failing to integrate its work, or being unable to buy
more implementation throughput.

Nor is the market observation itself differentiating. Git users broadly want
faster large repositories, better review, stacked changes, persistent change
identity, comprehensible conflicts, provenance, and less ceremony. ERSC's
existence validates that these are common problems, not that either company has
made a unique product bet. The architectural question is where those needs are
solved and which representation is canonical.

This document records the dilemma and the available seams so it is not lost
while day-to-day forge work continues. It is not yet a design for a replacement
VCS and does not presume that a particular competitive response has been
chosen.

## The dilemma

### Forge first, substrate later

Building the forge first provides concrete knowledge that a client- or
format-first project can otherwise miss:

- how review identity behaves across force-push, rebase, split, and squash;
- how CI, merge gates, attestations, and webhooks bind to observed source state;
- how permissions apply while graph state is moving;
- how publication is fenced, recovered, audited, and made durable;
- how indexes, snapshots, mirrors, forks, retention, and deletion consume the
  repository graph;
- where Git object identity is useful and where it is merely the only identity
  available.

The cost is accumulated coupling. Every feature that treats a branch name,
commit OID, or bare Git repository as the permanent domain model makes a later
conversion broader. Replacing the substrate underneath a complete forge can be
harder than adding conventional forge capabilities above a model designed for
stable changes and conflicts.

### Substrate first, forge later

A substrate-first project can make stable changes, revisions, conflicts,
operations, and high-throughput publication native from the beginning. It
avoids preserving accidental Git semantics merely because the first product
already exposed them.

Its risk is designing from local-client ergonomics or storage benchmarks before
encountering the complete server lifecycle. A technically elegant graph still
has to support authorization, review invalidation, CI identity, durable
publication, replication, audit, legal retention, deletion, migration, and
operational recovery. Those requirements can force another layer of papering
over if they are considered only after the format is established.

### Consequence

The strategic issue is not whether Gothab should imitate a particular client.
It is whether Gothab can continue shipping the Git-backed forge while a
parallel effort derives a canonical model from server requirements early
enough that current Git assumptions do not become permanent product contracts.

## Current boundary: `gothab://` is a seam, not yet a native model

Git discovers `git-remote-gothab` for a `gothab://` URL and communicates with
it through the remote-helper protocol. Gothab therefore already controls an
installed client/server boundary:

```text
Git -> git-remote-gothab -> Gothab services
```

The helper currently advertises Git-shaped `list`, `fetch`, and `push`
capabilities. Fetch requests carry Git OIDs, the helper delegates graph transfer
to an inner filtered `git fetch`, and the client installs Git objects and refs.
Push similarly uploads selected blob bytes out of band and delegates the
remaining graph and ref update to Git. The helper presently overrides
transport and physical blob integration; it does **not** override Git's
canonical graph or local object model.

That distinction should remain explicit. Existing `gothab://` support is not a
native VCS, but it avoids welding Gothab's long-term protocol to direct smart
HTTP or SSH. The same user-facing remote can evolve through stages:

```text
Today
  Git client
    -> Git remote-helper commands
    -> Git negotiation plus Gothab blob transfer
    -> Git-backed server repository

Possible intermediate state
  Git client
    -> Git remote-helper commands
    -> Gothab-native server protocol
    -> helper materializes a Git projection locally

Possible native state
  Gothab-native client
    -> Gothab-native server protocol
    -> native local change and working-copy model
```

This permits a server-native model to become authoritative before a native
working-copy client exists. Git can remain an adoption format and projection at
the edge rather than the source of every collaboration identity.

## Proposed design principle: derive from the server

A client-derived model tends to begin with local operations: amend, rebase,
undo, working-copy snapshots, and representing conflicts to one developer. A
server-derived model begins with durable shared truths:

- What is the stable identity of a logical change across revisions?
- What content and dependency state did a particular revision declare?
- What was proposed, published, superseded, or withdrawn, by whom, and under
  which authority?
- What exact state did review, CI, policy, and an attestation evaluate?
- How do unresolved conflicts remain representable and transferable rather
  than blocking all progress?
- How do many humans and agents publish concurrently without a single mutable
  ref becoming both lock and identity?
- Which operations are idempotent, replayable, and recoverable after partial
  failure?
- How are history, provenance, retention, deletion, and audit expressed?
- How is the same logical state projected to Git without allowing projection
  artifacts to redefine it?

Local ergonomics still matter enough to be a simultaneous design constraint.
The server's shared semantics establish the canonical contract, but a standalone
reference CLI must prove that a client can use that contract coherently and
without excessive round trips. If both the client and server workflows cannot be
expressed ergonomically against the same model, that is evidence that the model
is wrong rather than something the forge UI should conceal. Git compatibility
is one client adapter, not the constraint from which the server model is derived.

## Separate three decisions

"Build a new store" can collapse three different problems. They should be
decided separately and in this order unless evidence overturns it.

### 1. Logical collaboration model

Candidate concepts include stable changes, immutable revisions, dependencies,
conflicts, operations, publication records, workspaces, actors, provenance,
and projections. Their identity, mutability, ordering, and lifecycle must be
specified independently of byte encoding.

This layer determines what review, CI, permissions, merge policy, audit, and
retention refer to. If it is underspecified, those products will once again
construct missing identities around the storage layer.

### 2. Physical store format

The physical design covers content addressing, graph indexes, manifests,
deltas, replication, sharding, compaction, garbage collection, integrity, and
recovery. It should be justified by the logical operations and scale model,
not selected because it is a convenient representation for either Git or an
initial native client.

The format may reuse substantial Gothab infrastructure: globally addressed blob
bytes, locator-based placement, ownership manifests, repository routing,
fencing, publication receipts, and snapshots. Reuse is an implementation
question, not a requirement that the new graph retain Git object semantics.

The persistent repository must not become one serialized `Repository` value.
That struct is a reference model and algorithm harness. Physical state separates
immutable content-addressed objects from a tiny mutable repository head, and
large logical collections use persistent nodes so updating one path or change
reuses all unaffected structure. Loose objects are the write path; indexed sealed
segments are a transparent compacted representation for efficient object-store
storage and ranged reads.

### 3. Protocols and projections

A native protocol should transfer and publish the logical model efficiently,
with explicit versioning, resumption, authentication, and idempotency. Git
compatibility should deterministically project native state to commits, trees,
tags, refs, and packs where needed.

Projection raises product questions rather than only encoding questions. A
native unresolved conflict, operation history, stable change identity, or
many-to-one relationship may not round-trip losslessly through Git. The
contract must say which facts are preserved out of band, reconstructed, or
intentionally unavailable to an ordinary Git remote.

## Existing server-side evidence

Gothab already stores several truths richer than a Git repository can express:

- The durable push journal distinguishes preparation, ref publication,
  uncertainty, reconciliation, and ownership publication.
- Repository blob ownership and locator state separate required content from
  the incidental contents of one repository's local ODB.
- Pull requests and stacks preserve identities and relationships that survive
  changes to the commits and refs used to present them.
- Review comments are repositioned because line/commit coordinates are not
  durable review identity.
- CI runs, merge gates, review labels, CODEOWNERS decisions, and attestations
  evaluate a particular server-observed state with semantics outside Git.
- Search indexes, language statistics, snapshots, archives, mirrors, and
  package/security analysis are derived consumers coordinated after content
  publication.
- `repo_content_changed` already acts as a convergence boundary for mutations
  that alter what repository refs reach.

These are not proof of a complete replacement model. They are an inventory of
requirements and partial primitives produced by implementing the server first.
The parallel effort should extract their shared semantics instead of merely
moving Git commits and refs into a new database.

## Competitive considerations

ERSC is relevant because its announced direction compresses the period in which
Gothab could defer the underlying model without another product approaching
from below. Publicly known facts are limited: it describes a Git-compatible,
Jujutsu-oriented storage platform, early partner trials, high-throughput
publication, large-file support, fine-grained ACLs, and planned code review with
stacked diffs. Its private forge breadth, implementation maturity, benchmarks,
funding amount, customer traction, protocol, and operational behavior are not
known.

The analysis must therefore avoid two unsupported assumptions:

1. Sparse public UI or a statement that code review is "coming" does not prove
   that conventional forge capabilities have not been built privately.
2. Funding and more agents do not prove coherent execution, but Gothab cannot
   plan around ERSC suffering integration or quality failures.

The competitive danger appears when ERSC has *enough* forge above its substrate
for customers to choose the substrate's daily workflow advantages. It need not
match every issue, wiki, Pages, package, CI, or social feature before becoming a
direct alternative for teams that prioritize change identity, stacked review,
conflict handling, monorepo scale, or agent concurrency.

Conversely, racing by duplicating an entire funded roadmap feature for feature
would expose Gothab directly to the other company's parallelism advantage. A
server-native effort should therefore be evaluated for whether it preserves
Gothab's architectural agency and enables product behavior that cannot be
obtained by adding another Git-side table—not because a competitor has a named
engineer or because a new format is itself a differentiator.

## Strategic options

These are options for Bart to judge, not a decision embedded in this document.

### A. Keep the store deferred

Continue completing the Git-backed forge and rely on its broader product value
and immediate Git compatibility. This concentrates work but allows additional
Git-shaped contracts to accumulate and accepts the possibility that another
company establishes the native-hosting category first.

### B. Establish boundaries without building the store yet

Inventory Git assumptions and introduce domain interfaces so review, CI,
publication, and policy refer to explicit server concepts. This lowers future
coupling but risks speculative abstraction if no second implementation tests
the boundary.

### C. Run a narrow server-native prototype in parallel

Build one end-to-end change/revision/publication path, including Git projection,
review, CI, conflicts, recovery, and deletion. This tests the architectural
thesis without committing the whole product to a new format. It consumes
capacity continuously and can become an unshipped second system if its success
criteria are vague.

### D. Commit to a parallel native substrate

Treat the model, store, and protocol as a first-class product track alongside
the forge. This addresses the timing risk directly and permits architecture not
bounded by Git. It creates the largest coordination burden and requires a clear
boundary so ordinary forge work does not have to target two incomplete truths.

### E. Consume or interoperate with an external substrate

A future ERSC or other protocol could theoretically become a backend while
Gothab competes at the forge, CI, package, security, and hosting layers. No
suitable public product or stable protocol is currently known. Depending on a
direct competitor for Gothab's central substrate would also change strategic
control and incentives.

These options are not mutually exclusive over time. For example, B can make C
less invasive, and evidence from C can justify or reject D.

## Target architecture for the parallel effort

The parallel effort is only useful if the desired behavior is native to a new
repository format. A layer of native IDs in PostgreSQL over Git-canonical
commits would converge on the architecture Gothab already approximates with PR
identity, `Change-Id` footers, managed refs, and stacked reconciliation. It
would add a second transitional system without testing the central thesis.

The new core must therefore be able to create, revise, apply, compose, retain,
and publish logical changes with no Git repository present. Existing stacked
reconciliation is requirements evidence and a workflow corpus, not
implementation scaffolding that constrains the new format.

The first bounded prototype began on 2026-09-03. Its semantics are intentionally
narrow: changes have stable random identities; revisions are immutable authored
transformations; an application records a revision against an exact target view;
views are deterministic snapshots plus a change frontier; structured conflict is
a durable application result; and publication is compare-and-swap on view
identity. Dependency closures are composed explicitly and current changes are
applied in deterministic topological order, so independent roots remain visible,
sibling applications retain their declared targets, and multi-parent joins have
a constructed target view. Conflict resolution is scoped to one exact current
application and may alter only that application's conflict paths. Whole-file
edits and an unlocked, non-fsynced YAML metadata snapshot are disposable test
representations, not proposed storage or wire formats.

The next bounded slice adds byte retention without changing those identities.
`wheat-store` writes content under its native digest, verifies every read,
deduplicates repeated bytes, and reports missing and corrupt objects separately.
The CLI stores expected, proposed, and resolved bytes before committing metadata
and can retrieve them after reopening.

Views are now self-describing for the current prototype semantics: `ViewId` covers
the exact application selected for each change, the unresolved conflict IDs, and
the visible tree's required content IDs. `ApplicationId` covers revision, target,
and clean or conflicted result; `ConflictId` covers its target view and structured
terms. This lets repository code count or enumerate conflicts for an exact view,
change, or path and evaluate a pure no-unresolved-conflicts predicate without
replaying history. This is still not complete intrinsic ownership: the required
content set covers the visible tree but not every retained revision/conflict term,
no recovery reconciles metadata with content, and there is no garbage collector
or working-copy materialization.

A first canonical-object schema now lives at
`crates/wheat-core/schemas/wheat-objects.bs.json5`. It uses BinSchema to give every
field explicit binary framing rather than maintaining handwritten delimiter
encodings. Immutable tree, change, revision, application, conflict, view, and
content-manifest objects each begin with a two-byte `(object kind, encoding
version)` tag and are hashed over their complete canonical encoded bytes. Native
content references carry an algorithm tag; all map/set-shaped arrays must be
sorted and unique before encoding and are validated by `wheat-core`.

The schema is currently experimental. Until the **lock the native schema**
decision gate is checked, `V1` names and tags identify the current prototype
shape, not a compatibility promise: they may be changed in place, generated code
and fixtures may be replaced, and prototype repositories may be discarded. Do
not introduce `V2`, readers for obsolete layouts, or migrations merely because a
prototype `V1` changed. Once the schema is deliberately locked, incompatible
future encodings receive new object/version tags so persisted bytes and IDs do
not change in place; old readers and objects are retained according to the
compatibility and migration policy selected at that gate.

The prototype identity split is now explicit: `ChangeId` remains UUIDv7 because
it names stable collaboration identity across mutable intent, while `RevisionId`,
`TreeId`, `ApplicationId`, `ConflictId`, `ViewId`, and `ContentManifestId` are
BLAKE3-256 addresses of complete canonical object encodings. A revision therefore
does not encode its own ID. `ContentId` remains an algorithm-tagged digest of raw
file bytes. The checked-in generated Rust and its runtime are reproducible with
`scripts/wheat-gen-schema`, which refuses code generation from any BinSchema
checkout other than the recorded generator revision; handwritten delimiter
hashing is no longer used by Wheat model constructors.

The schema exposed a BinSchema Rust generator defect: a discriminated-union
variant containing nested structs emitted `ctx.extend_with_parent(...)` inside a
context-free encoder. The generator is now fixed at the source and has a focused
regression test, allowing Wheat to retain the natural top-level object union and
nested clean/conflicted application-result union instead of designing around the
tooling. The schema validates, generates Rust, and the generated crate compiles
against the patched local BinSchema checkout; production codegen and replacement
of handwritten IDs remain outstanding.

### Crate and dependency boundaries

The VCS and CLI are named **Wheat**. Crate names use the `wheat-*` prefix;
the installed reference-client binary is `wheat`. Dependency direction is the
contract:

```text
wheat-core
  pure identities, values, algorithms, conflict semantics, and store traits
  no gix, PostgreSQL, S3, Axum, gitd, or existing PR-model dependency

wheat-store
  immutable-object and publication-store traits, persistent collections,
  local loose-object/segment implementation, and content-manifest format
  depends on wheat-core; no Gothab database or object-router dependency

wheat-protocol
  versioned native wire types and workflow-level operations
  depends on wheat-core wire/domain values, not the store implementation

wheat-client
  local workspace plus remote-protocol client behavior
  depends on wheat-core and wheat-protocol

wheat-cli
  standalone reference binary installed as `wheat`; executable specification
  depends on wheat-client

wheat-git
  Git import/export/projection compatibility boundary
  depends on wheat-core, wheat-store, and gix; Git types do not cross into core

gothab-gitd (or successor repository daemon)
  native protocol host plus Git compatibility service
  depends on model/store/protocol and optionally the gix adapter
```

The native crates must not expose `gix::ObjectId`, Git refs, Git commit headers,
pack concepts, or commit-message footers as core identities. If the native model
cannot compile without gix, Git semantics have leaked back across the boundary.
Gix remains useful and already proven in Gothab for deterministic import,
projection, tree/commit construction, pack serving, and compatibility tests.

The CLI's remote mode must not depend directly on the production store crate.
Otherwise tests can accidentally use local capabilities absent from the wire
protocol. An in-memory or small local store adapter may implement the same model
for offline tests and local workflows.

### Native change workflow

The initial public operations should express the intended workflow rather than
translate Git commands one for one:

```text
create_change(base_view) -> ChangeId
revise_change(change, predecessor_revision, transformation) -> RevisionId
declare_dependencies(revision, changes) -> DependencyGraph
apply_revision(revision, target_view) -> Clean | Conflicted
compose_changes(target_view, dependency_graph) -> IntegrationViewId
publish_view(expected_current_view, integration_view) -> PublicationId
```

For a stack `A -> B -> C`, revising `A1` to `A2` must retain all logical
identities and automatically derive applications of `B` and `C` on the changed
parent results. A conflict is a durable successful application outcome, not a
half-completed rebase or an error state that prevents unrelated work.

The original target candidate made branch-like names mutable, policy-constrained
repository objects and routed every mutation through a fenced target advance. It
was intended to prevent forge-only policy bypass, but it also recreated branch
movement and encouraged a repository-global working/published view.

**Phase 2 follow-up (settled 2026-09-14):** replace that candidate with short
repository-local **labels bound to immutable views**. Labels identify useful views
without becoming change identity or mutable branch heads. A label created by Git
import retains the complete original Git ref as provenance even when its native
label is shortened; the ref remains import evidence, not native authority. A new
label defaults to the caller workspace's selected/effective view unless an exact
view is supplied.

Activating a label establishes that immutable view as the workspace basis.
Retained descendant views form its frontiers: zero selects the basis, exactly one
existing frontier auto-selects, and multiple require an explicit selection. No
operation may synthesize or combine all frontiers implicitly. Captured changes are
retained repository graph state and therefore reappear when switching back to a
basis/frontier that reaches them. Uncaptured filesystem edits are outside that
graph and block switching rather than being overwritten or transported.

The repository has no global workspace working view or published view. Each
workspace stores its own basis and selected frontier; its effective view is the
selected frontier when present and otherwise the basis. Label advancement and
explicit frontier consolidation, including any policy such as requiring a
conflict-free `main`, are deferred follow-ups rather than hidden effects of
capture, publication, or switching.

A revision preserves both its selected immutable edits and its materialized result:

```text
Revision {
    change_id,
    predecessor,
    authored_against,
    edit_ids,
    edit_transition,
    resulting_tree,
}

apply(selected_edits, authored_against.tree) == resulting_tree
```

`authored_against` is the change's **spot** in the view the author was looking
at: the view after every change before it in canonical order (dependencies
first, ties broken by the lowest change id). That includes earlier siblings the
change never declared as dependencies. Work written in a tree that already shows
them relies on them, just as it relies on its declared dependencies. Nothing
forces a change onto that base later: applying it to a view without those
siblings replays its edits there and records a clean result or structured
conflict. Creating, revising, resolving, and squashing all use this rule, and a
mutation that revises several changes authors each one at its spot in the new
composition. History import is the exception, since an imported commit records
its parents' tree, which is what its author saw.

The experimental V1 encoding implements this envelope. Text changes use guarded
fragment replacements; creation, deletion, binary/symlink replacement, and kind
changes use exact file operations. The authored view, exact resulting tree, and
revision/application distinction remain explicit. Edits are not promised to
commute or apply cleanly to every base. Application to a different target returns
an exact clean result or structured per-edit conflict evidence.

### Guarded edits and incremental review

**Implemented in the Wheat prototype as of 2026-09-07.** Wheat makes small,
individually reviewable replacements native rather than reconstructing their
identity from whole-file snapshots after each revision. The authoring boundary,
exact application rule, incremental identity preservation, VCS-level
consolidation, review continuity, and optional editor-preview boundary below are
settled requirements. Only the explicitly listed edge algorithms and concrete
encoding remain open. This section is the model's source of truth; no separate
interdiff design is required yet.

#### Immutable guarded replacements

A text edit has an expected fragment, replacement fragment, and exact surrounding
context. Conceptually (not a final schema):

```text
GuardedEdit {
    path,
    context_before,
    expected_fragment,
    replacement_fragment,
    context_after,
}
```

The path, expected fragment, replacement fragment, and exact context on both sides
all participate in immutable edit identity. An authored range may help locate or
inspect an edit, but fixed line numbers are not its application precondition.
Whether an explicit occurrence/provenance discriminator is additionally required
for identical definitions at distinct occurrences remains an edge decision below.
Unchanged edits must be preserved across revisions rather than regenerated with
arbitrary new hunk boundaries. Changing the replacement, expected fragment, path,
or required context creates a new edit identity and cannot silently inherit the
old edit's approval.

The complete `context_before + expected_fragment + context_after` must match
exactly at one location in the target file. Only the expected fragment is
replaced; context is preserved. No match is a conflict, and multiple matches are
an ambiguity conflict. Nearest-position guesses, whitespace normalization, fuzzy
matching, or fallback to weaker context must not turn a failed guard into a valid
application. Application outcomes retain exact result content and structured
conflicts as they do in the existing model.

Context is a correctness precondition, not merely a search aid. If an edit guards
an authorization check next to a function call and that check disappears, the
edit conflicts even if the function call alone is still unique. An author may
explicitly revise the guard to accept the new surroundings, but that is a new
reviewable edit, not automatic relocation of the approved one.

#### Default authoring and explicit wider guards

The default is **enough surrounding context to uniquely identify the edit in its
exact authored file, plus two additional lines on each side**, bounded by file
edges. This means two extra lines before and two extra lines after—not two total.
Capture the result once at authoring time and freeze those exact bytes in the edit;
do not recompute or shrink context when applying against another view. Uniqueness
is file-local, not repository-wide. Capture grows by complete lines symmetrically,
one available line per side per round, until the guard is unique, then includes two
further available lines per side. Captured file edges are identity-bearing anchors;
an insertion or replacement that remains ambiguous with the full file fails with a
typed ambiguity rather than using a tie-break as provenance.

The CLI must also allow an explicit larger identity/guard range containing the
replacement range and expose the captured bytes for inspection. For example, an
agent can replace lines 120–125 while requiring lines 108–132 to remain exactly as
observed. Those coordinates select authored content; they do not require the
edit to stay at those line numbers. This is particularly useful for agents that
can deliberately capture assumptions beyond the default neighborhood. CLI flag
syntax is not settled here.

Insertions require an unambiguous guarded boundary even though their expected
fragment is empty. Deletions retain their surrounding preconditions. Text is valid
UTF-8 without NUL and is captured with CRLF and standalone CR normalized to LF
before identity; final-newline presence remains content. File edges and empty files
use explicit anchors. Binary and symlink content stays byte-exact. Regular,
executable, and symlink kinds are represented in tree values; kind-only changes,
creation, deletion, and non-text replacement are explicit exact operations. Renames
remain deletion plus creation rather than inferred identity. No application path
performs relaxed matching or post-identity normalization.

#### Composition and revision/application separation

A guard is an edit's read precondition; its replacement region is its write
region. Two replacements can be non-overlapping yet interfere because one edits
context required by the other. **Agreed composition rule:** Wheat consolidates
such edits into one canonical, reviewable edit at the VCS layer, not in Gothab's
review UI. Sharing unchanged context alone is not interference. Canonical edits
are not an ordered script whose later steps silently bypass earlier guards.

Wheat does not require a background editing daemon or assign canonical IDs to
editor actions. Its functional baseline is to observe accumulated file
modifications when the user explicitly adds them to a change. Agent tool calls
and intermediate working-file states need not be captured. An optional daemon or
editor integration may continuously preview the guarded edits that the next add
would produce, show their current identities/review state, and warn when a nearby
modification would consolidate previously separate edits. This is a projection of
the exact same normalization library used by `wheat add`, not a second source of
truth or different functional model: canonical edits and IDs still result from
explicit addition, and add recomputes from the actual workspace instead of
trusting possibly stale preview state.
If a subsequent modification reaches an existing edit's replacement or required
guard, the preview must show that the old identity will be subsumed into a new
combined edit, with prior review evidence no longer current for that combined
edit. The likely editor boundary is an optional Wheat LSP server: it previews only
open/visible buffers and may use standard diagnostics, code lenses, and commands
plus custom requests where richer hunk rendering earns an editor extension. It is
not required to observe external tools or unopened files. Those files receive the
same canonical guarded-edit generation when they are explicitly added to the
change.

Adding work performs incremental normalization at the explicit add boundary:

1. Load the change's recorded edits and their last known application locations in
   the workspace baseline; compare that baseline with the current modified files.
2. Find the new modification ranges and relate them to existing edits, accounting
   for shifted coordinates rather than treating old line numbers as identity.
3. Preserve the exact definitions and IDs of unaffected existing edits—even if
   unrelated insertions shifted their locations. Do not regenerate the entire edit
   set from a fresh whole-file diff or accept a diff renderer's hunk split as identity.
4. Treat a new modification as affecting an old edit when it changes either the
   old replacement region or any byte of its required guard. Sharing guard bytes
   that remain unchanged is not interference.
5. Subsuming modifications replace every affected edit with a consolidated edit
   against the change's authored base, including unchanged text between affected
   regions. Capture its guard with the default rule while retaining any explicitly
   requested wider coverage. Repeat consolidation when guard growth reaches another
   edit. The result receives a new immutable identity.
6. Remove a net edit that has been completely undone. Historical revisions retain
   their old edits; only the new revision's selection changes.

For example, an existing `E1: foo -> bar` followed by a working-file modification
`bar -> baz` becomes `E2: foo -> baz`, not a canonical sequence of both edits.
Returning `bar` to `foo` removes the net edit. Unrelated edits retain their IDs
and review evidence even if their line positions shift. A consolidated edit
requires fresh review; constituent approvals do not automatically approve it.
No new edit identity is required before addition/normalization. Explicit wider
guards are supplied at this boundary through the CLI, not registered in advance.

Reapplying a descendant after an ancestor revision may find the descendant's
intended result already present. This is a successful authenticated no-op, not a
conflict: exact edits require full replacement `FileValue` equality, while guarded
text requires the replacement to match uniquely under the original context and
anchors. Application evidence retains the target value and guarded read range but
has no write range. Authored revision validation remains strict and cannot use
this outcome to publish an edit which contributed nothing at its authored base.

Consolidation assumes an intended final result is available, as with local
workspace additions. Canonical edits are not an ordered script: a later edit may
not silently consume text introduced by an earlier edit and thereby bypass its own
authored guard. Independently submitted incompatible edits from concurrent agents
therefore produce a conflict; Wheat must not invent their combined result. After
explicit resolution, the resolved effect becomes a new revision whose transition
references the conflicted application, failed edit IDs, and replacement IDs. The
implementation uses bounded deterministic LF-line mapping, rejects ambiguous
provenance, consolidates guard/write interference to a fixed point, and records
only component-proven subsumption. Unaffected definitions retain their IDs.

Keep stable changes, immutable revisions, exact authored views/results,
deterministic applications, and immutable retained views. Dependency movement
continues to create new applications of unchanged revisions rather than new
descendant revisions merely to encode a rebase. A revision should preserve its
constituent immutable edits, while an application exposes successful evidence and
its conflict object exposes failed evidence, both keyed by canonical `EditId`, plus
the exact overall tree/conflict result. Canonical collections are sorted by raw
content-derived IDs and decoding rejects duplicates or noncanonical ordering.

This model does not promise that every agent tool call becomes a separate edit,
that arbitrary edits commute, or that a textual match establishes semantic
correctness. Tool mutations and logical changes remain distinct as described in
the following section. Split/combined edits need explicit lineage rather than
assuming that a diff renderer's hunk partition is durable identity.

#### Interdiffs and individual review evidence

An interdiff should distinguish preserved edits from added, removed, and modified
edits, separately from changes in their application context. Given revisions
`B1 = [E1, E2, E3]` and `B2 = [E1, E2, E3, E4]`, previously reviewed E1–E3 remain
recognizable and E4 is new. Replacing E2 with a different definition E2' requires
fresh review of E2'; it does not reset unrelated unchanged edits.

Gothab owns reviewer identity, viewed/approved state, and merge policy. Wheat
provides immutable edit definitions and exact application evidence. Review
records need to identify the edit and the application/context actually observed.
An unchanged edit whose exact guard remains uniquely valid can retain individual
review evidence; ambiguity or guard failure cannot be represented as current valid
approval. Added or modified edits require review; changing a guard is a modification.
A consolidated edit receives a new identity and constituent approvals do not
implicitly approve it, while independent E1/E3 approvals survive replacing E2 with
E2'. Historical evidence is retained rather than rewritten. Removal is recorded in the
immediate revision transition; later reintroduction of an identical content-derived
definition is recorded as newly added and does not silently restore approval.
Whether all current individual approvals suffice to authorize a new publication is a separate
forge policy decision, not a conclusion of exact matching. CI remains tied to the
exact integration/application state it tested.

Broader review-context warnings are separate from guard validity. Language-aware
analysis may report that an enclosing function, type, or module changed since
review even when the edit and its guard still apply. Tree-sitter can supply syntax
boundaries for supported languages; it does not establish semantic equivalence.
Parser versions, unsupported languages, or missing analysis must not alter native
edit identity or exact application results. Such warnings must remain visibly
separate from application conflicts and from claims that an edit itself changed.

Historical edit definitions, guards, application outcomes, and their required
content must survive retention and reopen so old reviews/interdiffs remain
reproducible. This extends the existing immutable closure/retention contract; it
must not depend on old file bytes happening to remain cached. Ordinary Git imports
currently synthesize one change per commit OID; inferring rewrite identity and
preserving native edits through future Git round-trips is not already solved by
adding guarded edits.

#### Acceptance corpus

Before treating the new model as implemented, exercise these cases through core
application and persisted-store reopen, plus CLI tests for authoring controls:

1. Sixteen independent guarded replacements in a 500-line file remain individually
   identifiable; an edit outside their guards does not invalidate them.
2. Moving an exact unique guarded block to different line numbers still permits
   matching; missing or duplicated full guards produce explicit conflicts.
3. Removing an authorization check inside required context conflicts even when
   the expected replacement fragment itself still occurs uniquely.
4. The default adds two lines on each side after uniqueness, clips at file edges,
   and produces deterministic guards; explicit wider ranges are retained exactly.
5. Insertions, deletions, empty files, missing final newlines, repeated text, and
   the selected byte/line-ending rules have unambiguous outcomes.
6. Explicit additions consolidate replacement/guard interference at the VCS layer,
   including transitive interference after guard expansion; shared unchanged
   context alone does not consolidate edits. Concurrent incompatible submissions
   conflict rather than inventing a combined result. Independent replacements do
   not depend on incidental iteration order.
7. Adding E4 retains evidence for unchanged E1–E3; changing E2's guard or content
   requires fresh review without resetting E1/E3. Removing, splitting, combining,
   or reintroducing edits exercises the chosen lineage/carry-forward policy.
8. Moving dependency A reapplies B without changing B's authored edit identities;
   conflicts and any explicit resolution are visible rather than hidden as rebase
   noise. Previously tested integration state is not confused with the new one.
9. Reopen reproduces historical edit definitions, results, conflicts, and interdiff
   inputs. Missing required historical content fails verification.
10. A supported-language enclosing-function change can produce a separate review
    warning while the exact edit remains applicable. Parser failure or unsupported
    language does not change native application correctness.
11. Without observing intermediate editor/tool actions, add `foo -> bar`, modify
    the file to `baz`, and add again: the new revision contains `foo -> baz` with
    a new ID. Undoing back to `foo` removes the edit. Unaffected edits retain exact
    IDs despite line shifts, and historical revisions remain readable after reopen.
12. An optional LSP/editor preview predicts the same canonical edit set as an
    immediate explicit add for an open buffer. Moving a new modification into an
    old edit's guard visibly changes the preview to a consolidated new identity;
    stopping the LSP does not affect subsequent add semantics or repository state.
    An externally modified or unopened file still receives guarded edits normally
    when explicitly added, without requiring prior observation.

#### Settled guarded-edit decisions

These are requirements, not implementation defaults left to choose later:

1. Canonical edit identity covers path, exact expected and replacement bytes, and
   exact captured context before and after. Authored coordinates are only hints.
2. Application requires exactly one match of the complete captured
   `before + expected + after`; zero and multiple matches are typed durable
   conflicts. There is no fuzzy, nearest, normalized, or weaker-guard fallback.
3. Default authoring captures the minimum context needed for file-local uniqueness,
   then two additional lines on each side, clipped at file edges and frozen.
   Explicit add-time wider ranges are retained exactly and remain inspectable.
4. Canonical edit IDs arise only at explicit addition. Editor/tool operations and
   intermediate file states have no canonical identity. Optional LSP preview is
   advisory and invokes the same normalization logic as add.
5. Updating a change is incremental: unaffected edit definitions and IDs survive;
   modifications to an edit's replacement or guard subsume it into a new edit;
   a completely undone net edit disappears from the new revision; history remains.
6. Guard-interfering local modifications consolidate at the VCS layer against the
   common authored base, including unchanged text between them. Shared unchanged
   context is not interference. Consolidation repeats transitively after guard
   growth and yields a new identity requiring fresh review.
7. Canonical edits are not ordered scripts. Concurrent incompatible submissions
   conflict rather than being silently sequenced or combined. An explicit resolution
   may subsequently be represented as a new consolidated edit.
8. Unchanged edit identity carries individual review continuity; changed guards,
   replacements, and consolidated edits do not inherit approval. Broader
   tree-sitter-derived enclosing-scope changes are separate review warnings and
   never alter native identity or exact application validity.

#### Implementation decisions confirmed after the model review

The native guarded-edit workflow is the next implementation priority, before
further large-history performance work. This milestone includes canonical edits,
exact application/conflicts, incremental explicit add, historical edit evidence,
retention/reopen, and both Git import paths. LSP integration, structural review
warnings, and forge approval/publication policy remain separate follow-ups.

Text capture classifies valid UTF-8 without NUL as text and converts both CRLF
and standalone CR line endings to LF before computing native content and edit
identities. It preserves whether a final newline exists and performs no other
whitespace normalization. Binary content remains byte-exact. Exact matching then
operates on canonical text; it never retries a failed match with normalization or
weaker guards. Consequently, a Git import preserves original commit provenance
but its native text tree is the normalized tree, not necessarily the original
Git blob bytes.

Captured file-start and file-end boundaries are explicit identity-bearing guard
anchors. A target must satisfy those anchors as well as the captured bytes.
Native tree entries will also represent regular, executable, and symlink kinds;
mode-only changes are explicit operations. Symlink targets remain byte-exact and
are not passed through text normalization. These decisions supersede the former
encoding and file-edge alternatives below and are implemented by the current
experimental Wheat prototype.

#### Guarded-edit implementation boundary

The prototype implements the previously open edge choices as follows:

1. **Deterministic context expansion.** Complete lines grow symmetrically until
   unique, followed by two extra available lines per side; file edges are anchors and
   irreducible ambiguity is a typed failure.
2. **Range mapping and transitive consolidation.** Bounded deterministic LF-line
   alignment maps the applied baseline to the workspace. Ambiguous alignment fails;
   guard/write-connected components merge to a fixed point while explicit wider
   coverage remains a lower bound.
3. **Occurrence and lineage.** Canonical definitions are content-derived without an
   occurrence salt. Exact anchored matching distinguishes occurrences. Revision
   transitions record preserved/added/removed definitions, component-proven
   subsumption, and conflict-resolution evidence. Rename remains delete plus create;
   removal/reintroduction is discontinuous for review continuity.
4. **Text and non-text encoding.** Guard ranges are canonical UTF-8 byte ranges after
   CR/CRLF-to-LF capture. Final newline and anchored empty-file boundaries remain
   identity. Exact operations cover create/delete, binary/symlink replacement, and
   regular/executable/symlink kind changes.
5. **Resolution evidence.** Resolution transitions reference the conflicted
   application and failed edit IDs plus any replacement edit IDs. Forge structural
   warnings and approval/publication policy remain separate.
6. **Bounded failures and storage.** Capture, diff, matching, edit count, resolved
   bytes, generated output, and aggregate work have explicit limits and typed
   failures. Standalone edit objects and per-edit evidence are canonical V1 records;
   permanent per-character identity is not used.

### Operations are not logical changes

An observed filesystem mutation and a reviewable logical change have different
cardinality. One agent task can contain many edit, write, formatter, generator,
and shell-process mutations. Conversely, one broad process invocation may
change files for several independent concerns that are later split.

The earlier operation-capture sketch below is optional provenance, not a
requirement to observe filesystem/tool mutations continuously. The agreed client
workflow captures accumulated modifications at explicit addition to a change,
as specified above. If a client supplies causal operation metadata, it may link
those operations to the captured effects without making them canonical edit IDs:

```text
Change
  -> immutable Revision
       -> one or more mutation effects since its predecessor

ProcessMutation
  -> file effect 1
  -> file effect 2
  -> ...
```

A formatter or script changing thousands of files is one causal process
operation with many exact effects. Whether those effects constitute one logical
change depends on intent. The model must permit whole-operation, per-file, or
range-level reassignment when splitting work, without rewriting the fact that
one process originally produced them. Applying elsewhere uses recorded effects,
not blind command replay; a reproducible command/tool/version recipe may be
retained as explanatory metadata.

Agent edit operations map conveniently to mutation effects, but perfect
character-level attribution is explicitly not a foundational requirement.
Revision-, file-, or hunk-level links to agent sessions can be added according
to demonstrated product need without turning every repository into a CRDT or
charging every operation for permanent character identity.

### Shared content is intrinsic to the format

Git's repository-local ODB model forced Gothab to add blob extraction,
alternates, locator state, ownership manifests, push-journal pins, and
convergence around it. The native format must instead refer directly from trees,
revisions, conflicts, and retained views to globally addressed immutable
content:

```text
TreeEntry::File { content: ContentId, mode }
RepositoryView { root_tree, change_frontier, content_manifest }
RepositoryState { changes: ChangeMap<ChangeId, ChangeObjectId>, ... }
```

The canonical change map provides bounded stable-ID lookup without scanning the
append-only retained-change inventory. Retained sets remain the historical
reachability authority; the map indexes each retained logical change exactly once
and complete validation proves key/object identity and inventory coverage.

`ContentId` is native and algorithm-tagged. Git SHA-1/SHA-256 blob IDs are
projection identities computed by the compatibility adapter, not native content
identity.

The repository format owns **what** content a view and all retained native
objects require. The object router owns **where** durable and cached copies live.
Bucket, backend, and gitd-host placement must not enter native repository
identity.

Every retained view reachable from repository state must commit to an immutable,
incrementally transitionable content manifest. Required content includes at least
visible tree closure, retained revisions, unresolved conflict terms,
snapshots/archives under policy,
and compatibility projections while leased. Publication ordering must guarantee
that a crash may over-retain bytes but cannot expose a view whose required
content is reclaimable:

1. write immutable content, trees, transformations, revisions, and conflicts;
2. construct the proposed repository view and content-manifest transition;
3. durably retain newly required content;
4. compare-and-publish the view under fencing;
5. expose it to readers;
6. release removed content conservatively and asynchronously;
7. derive Git and other disposable projections.

This makes sharing, deduplication, and safe retention indelible properties of
the format instead of a separate post-receive repair effort.

### Object-store-native physical layout

The in-memory `Repository` is an executable model, not the persistent storage
unit. Wheat separates two backend capabilities:

```text
ImmutableObjectStore
  put_if_absent(ObjectId, canonical bytes)
  get(ObjectId)
  exists_many(ObjectId[])
  optional ranged reads and batch reads

PublicationStore
  read repository head
  compare-and-advance(expected generation/state, proposed generation/state)
```

The immutable store holds directory/tree nodes, revisions, transformations,
applications, conflicts, views, content manifests, publication records, and
immutable index segments. Raw file content uses its separately algorithm-tagged
`ContentId` namespace and may share the same backend without being decoded as a
`WheatObject`. The publication store owns only tiny mutable roots. Deployments
may combine them differently:

| Deployment | Immutable objects | Mutable publication head |
|---|---|---|
| Local CLI | Filesystem | Locked local pointer file |
| Gothab | `ObjectRouter` over configured backends | PostgreSQL CAS |
| Single-writer daemon | Local cache plus S3/R2 | Memory plus durable journal and checkpoints |
| Object-store-only | Object storage | Conditional ETag/version CAS, only where semantics are strong enough |

The first remote immutable adapter targets Apache Arrow's generic Rust
`object_store` interface rather than Gothab storage types. Its S3 configuration
accepts endpoint URL, bucket, region, access key, secret, and key prefix, and uses
conditional create rather than overwrite for immutable objects. One shared
conformance corpus runs against the adapter's in-memory backend and a pinned,
throwaway SeaweedFS S3 service; the latter also publishes a complete repository
closure before advancing a separate local publication head. This is the intended
early abstraction test: S3-compatible backend behavior is qualified before
adding `ObjectRouter`, locator rows, or PostgreSQL. Provider adapters must still
verify Wheat's BLAKE3 canonical identity on reads because backend checksums are
transport evidence, not native object identity.

`scripts/wheat-object-store-bench` runs the same adapter against memory and the
pinned SeaweedFS service and emits a JSON report under `target/`. It measures
cold conditional creation, idempotent duplicate publication, verified reads, and
`exists_many` for both many tiny revision objects and fewer larger tree objects,
with configurable object counts, tree width, batch size, and concurrency. This is
an exploratory comparison rather than a performance gate: localhost SeaweedFS is
not production S3 latency, and results are expected to change as batching and
segments replace per-object requests. The first run already exposes the intended
architectural pressure: small remote objects are dominated by request/setup
latency, while larger payload throughput is much closer to memory; the current
`exists_many` implementation performs one HEAD per ID and therefore needs true
concurrency or a backend-specific batch strategy before large closures.

A memory-backed publication store must declare its durability. Periodic
checkpoints alone are process-local or asynchronously durable: a crash can lose
an acknowledged advance. A durable configuration appends and flushes a small
fenced repository-head-advance record before acknowledgement, updates memory, and
later compacts the journal into an immutable checkpoint. Recovery loads the newest
checkpoint and replays later records. Multiple writers additionally require an
ownership epoch or another fencing mechanism; routing to one repository owner is
an optimization, not correctness by itself.

Repository publication reduces repository graph state to one mutable CAS value.
The earlier sketch included target and policy maps; after the Phase 2 label decision,
the head instead reaches immutable label bindings and retained graph roots, never a
global workspace working/published view:

```text
RepositoryHead { generation, repository_state_id }
    -> immutable RepositoryState
         -> label-map root          // short name -> immutable view + provenance
         -> retained graph roots
         -> previous state/publication

WorkspaceState { basis_view, selected_frontier? }
```

Workspace state is per workspace rather than part of the repository head. All
immutable objects are written and verified before `RepositoryHead` advances.
The first local `PublicationStore` uses an advisory file lock, an exact
`RepositoryHead { generation, repository_state_id }` comparison, a fixed
versioned head encoding, and temp-write/fsync/rename/directory-fsync publication.
Concurrent publishers therefore have one winner, stale and skipped generations
fail without changing the head, and reopening observes either a complete old or
complete new head.

`RepositoryStateV1` and deterministic closure construction now retain every
observable view, revision, and application plus their trees, conflicts, and
per-view manifests. The state also references an aggregate content manifest that
covers visible and historical trees, revision edit terms, and conflict terms.
The local publisher writes and verifies the complete immutable closure and every
required raw-content object before initializing or advancing the head. Missing or
corrupt content leaves the head unchanged; a stale CAS leaves only harmless,
retryable immutable objects. The local CLI now uses the canonical head directly:
it reopens loose and packed stores through root-backed readers, performs daily
copy-on-write mutation without reconstructing retained history, and rejects the
obsolete disposable-YAML prototype format.

A failed or partial upload stays unreachable; retrying writes the same bytes under
the same ID. Bucket names, storage backend IDs, segment offsets, and locator rows
never participate in logical object identity.

#### Persistent trees and maps

A view should remain small even when the repository is large:

```text
ViewV1 {
  tree: TreeId,
  revisions: RevisionMapId,
  applications: ApplicationMapId,
  unresolved_conflicts: ConflictIndexId,
  required_content: ContentManifestId,
}
```

Each root names an immutable Merkle collection. Trees are directory nodes (with
deterministic shards for exceptionally wide directories), so changing
`src/auth.rs` writes the content, its containing directory path, and a new root
while reusing every unaffected subtree. Revision selections, application
selections, conflict indexes, and content manifests likewise use explicit
persistent node types rather than whole arrays in every view. A one-entry update
therefore rewrites bounded paths and old views continue sharing unchanged nodes.
A qualification import of `terraform-provider-aws` makes this requirement
concrete: its 134,273-commit `main` graph is discovered in about 0.75 seconds,
but the current flat-tree importer reaches a 16 GiB safety limit during tree
preparation after roughly 43 seconds. It cannot safely retain one complete flat
tree and frontier per historical commit; persistent nodes and streaming
publication are prerequisites for imports at that scale.

These collection types should be explicit in BinSchema—`RevisionMapNodeV1`,
`ApplicationMapNodeV1`, `ConflictIndexNodeV1`, `ContentManifestNodeV1`, and
`DirectoryNodeV1`—rather than one universal generic map whose evolution couples
unrelated domains. The first disk-backed slice now implements canonical directory
nodes under the unlocked tree V1 tag, a validated domain-level decoder, and an
async `MerkleTreeStore` over the generic immutable-object boundary. Path edits
rewrite only touched ancestor directories, prune empty directories, and preserve
unaffected subtree IDs; conformance runs against both the loose filesystem and
Apache in-memory adapters. A typed fixed-depth byte-radix revision map now also
provides bounded-fanout, copy-on-write selection updates through the generic
immutable store, including reopen, replacement, deletion, idempotency, and
unaffected-branch sharing tests. All five typed collection families now share the
store's copy-on-write engine. Persisted views reference revision/application maps,
conflict sets and small content manifests wrapping content-set roots; persisted
state references four retained-object inventories. Synchronous sink-based builders
produce the same roots as incremental store construction and drive both executable
identity calculation and reference closure emission, without adding root fields to
`Repository` or caching a second closure per hydrated view.

`RepositoryReader` decodes lazy typed records and verifies retained inventories
using verified object reads before root-only publication/CAS. It checks historical
edit/conflict content against the aggregate content set as well as raw bytes;
HEAD/existence responses are not accepted as integrity evidence. A fresh per-run
SQLite scratch index proves retained/content membership and records context-scoped
receipts for shared trees and selection/content radix subtrees; a byte- and
entry-bounded authenticated-object cache avoids repeatedly decoding immutable nodes
without carrying trust across validation calls. Directory depth remains capped at
1024 and wide nodes are decoded whole. The resumable Git importer bypasses executable
hydration, retains all pinned branch history, batches final inventories through a
disk accumulator, and publishes a canonical branch envelope. SSEM and BinSchema are
performance regression fixtures, not large-history qualification; wider/deeper
repositories, remote ranges, and incremental cross-publication verification remain.
Derived indexes such as reverse dependencies or path-to-
conflict lookup may be immutable rebuildable segments; they accelerate queries
but never become repository truth.

#### Loose objects and indexed sealed segments

One backend key per tiny object is a correct baseline but inefficient at scale.
Wheat therefore allows the same logical object to be stored loose or in an
immutable sealed segment. Location is physical metadata:

```text
ObjectLocation {
  segment_id,
  offset,
  stored_length,
  logical_length,
  codec,
}
```

`ObjectId` remains the hash of the canonical BinSchema bytes; compaction,
replication, and movement between loose storage, local NVMe, S3, and R2 do not
change it. Small metadata and content below a measured threshold are segment
candidates. Large files remain ordinary tree entries addressed by one whole-file
`ContentId`; changes above guarded-authoring bounds are exact file replacements,
not rejected or split into independently editable pieces. The content backend may
physically retain that identity as a whole object or as a manifest of
content-defined chunks. Chunking is storage and transfer metadata: reassembly must
authenticate to the same whole-file `ContentId`, and changing physical layout must
not change any tree, edit, revision, application, or view identity. Native
streaming put/get and bounded FastCDC-style chunk storage remain to be designed;
the current prototype's `Vec<u8>` content interface and loose large-object fallback
are only the reference implementation.

A sealed segment is footer-indexed. A tiny fixed prefix may identify the file as
a Wheat segment, but contains no offsets and never needs backpatching. Object
payloads are written first, followed by paged sorted indexes, a compact page
directory/fan-out structure, and a fixed-size authoritative footer:

```text
[tiny identification prefix: magic, format family]
[object payloads]
[paged ObjectId -> kind, offset, stored/logical length, codec index]
[compact index-page directory, fan-out, and optional Bloom metadata]
[fixed footer]
  magic, version, segment length, segment ID, object count
  index/directory offsets and lengths, payload/index digests
```

This permits natural one-pass construction: stream payloads while recording
locations, emit the index and directory, then append the footer. A valid footer is
the segment's completion marker; a truncated upload is not a sealed segment. The
same layout also supports bounded **active-pack growth on local disk** before the
file is sealed: retain the existing payload prefix, truncate at the footer's
`index_start`, append new payloads, regenerate one cumulative sorted index and
page directory for all retained and new payloads, hash the resulting body again,
and write one replacement footer. This is not a chain of per-append footers or
embedded mini-segments; the latest footer describes the complete current file, so
lookup remains one index traversal rather than searching historical tails.

The active pack is process-exclusive mutable physical state, not a logical Wheat
object. Each completed footer publishes a durable generation that readers may use,
but the container does not become a final immutable segment until it reaches the
sealing threshold. Existing payload offsets stay stable while it grows, allowing
the derived SQLite locator to retain old rows and add only new locations after a
successful append. The segment body digest and published physical generation
change whenever the cumulative tail is rewritten; no repository tree, edit,
revision, application, view, `ObjectId`, or `ContentId` changes with it.

An in-place append must be recoverable without relying on a partially rewritten
footer. Before truncating the old metadata tail, durably journal the active-pack
identity, old total length and `index_start`, the old index/directory/footer bytes,
and the intended new generation. Then append payloads and the rebuilt tail, sync
the pack, atomically advance its small physical head/checkpoint, and update locator
rows plus that same checkpoint in one transaction. Only after all objects for a
bounded import batch are covered by a durable active-pack generation may the
corresponding import journal entries be acknowledged together; per-commit
acknowledgement must not force a pack rewrite. Recovery either validates the
completed new footer or restores the saved old tail at its original `index_start`;
bytes after the last published length are ignored. The isolated atomic-import path
may use deferred syncs inside its hidden staging directory, but it must close the
pack and locator before the terminal filesystem sync and destination rename.

Active packs are size-bounded by measured rewrite cost rather than the current
8 MiB ingestion constant. Rebuilding and hashing the cumulative tail is expected
for each append; once that cost is no longer appropriate—an initial qualification
target is in the 100–200 MiB range—the writer seals the file as an immutable
segment and starts another active pack. The threshold is a physical policy and is
not part of logical identity. A remote object store that cannot append receives
only sealed packs; local active packs may later be uploaded when sealed.

The first implementation may still stage a sealed pack to a local file before one
immutable upload, but its format does not require payload backpatching and can map
to multipart object storage without changing logical objects.

The physical segment descriptor records its stored length, so a reader can issue
one suffix range request without a preliminary HEAD. The normal tail fetch
(roughly 16–64 KiB, qualified by measurement) should contain the footer and the
complete top-level page directory. A cold lookup then range-reads the relevant
index page and object; cached directories/pages reduce later lookups to the object
range alone, and nearby payload ranges may be coalesced. Very large indexes are
never required in full.

`wheat-store` owns range planning behind a small reader capability implemented by
local `pread`, in-memory slices, S3/R2 range requests, or Gothab's existing
`ObjectStore::get_range_stream`. BinSchema defines segment headers, index pages,
records, and footers, but generated domain objects do not know their physical
location. Local active-pack publication is:

1. encode canonical objects, determine IDs, and discard keys already present;
2. durably journal the old tail and proposed append generation;
3. truncate the old index/directory/footer tail, append payloads, and emit a
   cumulative index, directory, body digest, and replacement footer;
4. sync and atomically publish the new active-pack generation;
5. transactionally add locator rows/checkpoint;
6. acknowledge the bounded batch of import receipts covered by that durable generation; and
7. at the configured rewrite bound, seal the active pack and create a fresh one.

Sealed-segment publication or remote upload is:

1. verify the complete sealed pack and its final identity;
2. upload/install it as one immutable object;
3. publish an immutable segment catalog referencing it;
4. advance the repository state/head with CAS; and
5. reclaim superseded loose objects or segments only after retention permits it.

The segment catalog itself is immutable and can use fan-out metadata and Bloom
filters to skip irrelevant sealed packs. Plain minimum/maximum ranges do not prune
publication-grouped packs of uniformly distributed cryptographic IDs; range
partitioning is useful only if a later rewrite intentionally creates non-overlapping
key ranges. The local SQLite locator is persistent derived acceleration tied to the
physical catalog/active-pack checkpoint: it routes an object ID directly to a pack
and stable payload range, but immutable pack indexes and catalog records remain the
rebuild authority. Ordinary active-pack growth is not compaction and must not create
one catalog record or physical file per commit. A later sealed-pack rewrite may
combine old packs or repartition ranges without changing logical object or view
identity; catalog roots remain directly addressable so locating them never requires
the catalog being located. Sealed-pack deletion remains disabled until old-catalog
retention, reader retry, snapshot, replication, and lease semantics are implemented.

Loose unpublished objects use a simpler initial safety rule: an unreachable
object is ineligible for collection until at least 24 hours after the storage
backend's `last_modified` time. Missing or untrustworthy timestamps fail closed by
retaining the object. Publication verifies every newly required object immediately
before its fenced head advance; a preparation older than the grace interval must
re-upload anything missing. Explicit in-flight pins can be added later for
long-running workspaces, but are not required for the first collector. This
matches Gothab's existing age-aware `list_prefix_with_meta` capability and ensures
a concurrent reachability scan cannot delete freshly uploaded publication input.

### Git compatibility and repository daemon

The first importer is intentionally asymmetric and deterministic. Every reachable
Git commit becomes one synthetic Wheat change and one immutable revision. Root
commits apply to the empty view; non-root commits use only their first parent as
the semantic Wheat dependency and encode the exact first-parent-tree-to-commit-
tree transformation. Additional merge parents remain ordered import provenance,
so the imported merge result stays clean without pretending Git preserved its
conflict-resolution process. Local branch names map to separately composed Wheat
views; shared commits and content deduplicate across those views.

Canonical change metadata includes title, description, creator, creation time,
and optional foreign provenance. Revision metadata includes the full message,
author and committer identities/timestamps, and provenance with the source object
ID and ordered parent IDs. Git imports populate both. This prototype currently
requires UTF-8 metadata and paths, preserves regular/executable/symlink kinds,
normalizes canonical text, retains binary/symlink targets byte-exactly, and rejects
submodules. Imported change IDs are deterministic UUIDv5 values namespaced by Git
commit identity, while Git OIDs remain provenance rather than native authority.
The reference in-memory importer has explicit branch, reachable-commit, flat-tree,
path-byte, and pending-content limits and fails with typed errors before exceeding
them. The staged importer journals bounded discovery and materialization batches,
uses visible-only view manifests plus a separate historical aggregate inventory,
and resumes packed or loose imports with authenticated mapping replay. Both paths
derive identical edit, revision, application, and view identities. Content is
hashed and deduplicated during traversal, then published through bounded parallel
durable writes rather than serially waiting for one fsync per blob. Interactive
edits retain the conservative clone-and-rebuild path needed for atomic single-change
mutation; staged imports do not replay the entire accumulated history after every
commit. As an earlier qualification point,
importing the 238 reachable commits and 2,239 unique content objects in the local
BinSchema repository completes in roughly two seconds on the development
machine (plus roughly 0.6 seconds to enumerate and encode its complete canonical
closure).

`wheat-git` owns the complete impedance mismatch:

- import Git blobs/trees/commits into native content, trees, changes, and views;
- create native changes where Git supplies no trustworthy identity;
- use explicit mapping when a Git projection can unambiguously identify an
  existing native revision;
- generate deterministic Git blobs, trees, commits, refs, and packs from clean
  native applications;
- retain native-to-projected-ID mappings outside the native store's identity
  model;
- report information loss instead of treating an editable footer as canonical.

Gitd can initially serve a disposable projected bare repository/cache. Later it
may synthesize packs directly from native views through the adapter. In
parallel, it (or a renamed successor) serves the native protocol directly from
the native store:

```text
native client -> native protocol -> repository daemon -> native store
Git client    -> smart Git       -> repository daemon -> gix adapter
                                                   -> native store
```

A repository using the native format has one canonical store. Git objects and
refs are projections or import inputs, never a silent dual authority. Existing
routing, mutation admission, fencing, publication recovery, locator placement,
and migration work supplies invariants to preserve, but its Git-specific schema
and control flow are not copied into the format by default.

## Reference CLI as executable specification

A standalone CLI is part of Phase 1, not polish after server integration. It
exists to prove that the same model is ergonomic from both sides. Internal Rust
APIs can hide chatty protocols, concepts no user can name, and conflict states
that only a forge UI can repair.

The detailed local workspace, command, configuration, lifecycle-tooling, and
acceptance contract lives in
[`wheat-local-cli.md`](./wheat-local-cli.md). This section retains the
architecture-level requirements shared with the future remote client.

The CLI should support two modes with matching semantics where applicable:

- **Local mode:** a native repository/workspace backed by a local test store,
  suitable for fast experiments and offline work.
- **Remote mode:** the same workflows through the real versioned protocol,
  exercising authentication, resumability, latency, concurrency, and server
  authority.

The CLI should make these questions easy to answer without manually assembling
change, revision, application, view, and tree IDs:

- What logical change am I editing?
- What does it depend on, and what depends on it?
- What changed since its preceding revision?
- What is its current application result on the target view?
- Why is it conflicted, and can I resolve or delegate it?
- Which descendants were automatically updated?
- Which exact result did CI/review evaluate?
- What is published, remote-only, or still local?

A first black-box workflow should cover:

1. initialize a native repository and publish a base view;
2. create dependent changes `A -> B -> C`;
3. revise `A` and observe stable identities plus automatic `B`/`C`
   applications;
4. persist a conflict in `B` without aborting the operation;
5. resolve it and continue applying `C`;
6. apply one revision to two unrelated target views;
7. synchronize the graph through the native protocol;
8. export a selected clean view to Git and pass `git fsck`;
9. import an ordinary Git change while exposing lost native identity honestly.

The same corpus should run against in-memory/local and client/server backends.
Pure model property tests cover deterministic application and graph invariants;
CLI black-box tests cover usability and output; protocol tests cover interruption,
stale publication, concurrent revision, server restart, incremental transfer,
and authentication failure. If the workflow requires force-pushes, temporary
branches, footer repair, or routine use of opaque application IDs, the native
abstraction has not replaced Git's workflow.

## Prototype acceptance questions

A prototype should answer architectural questions, not merely serialize a new
object type. A useful narrow slice would need to demonstrate:

1. A logical change receives a stable identity independent of any Git OID.
2. Two or more immutable revisions of that change can be published.
3. Amend, rebase, or dependency movement changes the Git projection without
   replacing the logical change or losing review history.
4. Review comments, approvals, CI results, and attestations bind to an explicit
   revision and have defined carry-forward/invalidation behavior.
5. An unresolved conflict is durable shared state and can be published,
   inspected, transferred, and later resolved.
6. Concurrent publishers receive deterministic outcomes without relying on a
   mutable branch ref as the only serialization or identity primitive.
7. Every accepted native state has a deterministic Git projection sufficient
   for an ordinary Git clone/fetch, with documented lossiness.
8. Publication is idempotent and crash-recoverable across metadata, content,
   projection, and derived-task boundaries.
9. Snapshot/restore, indexing, retention, and deletion operate from native
   identities and cannot prematurely reclaim required content.
10. A standalone CLI can perform the complete workflow against both a local
    store and the real remote protocol without access to internal server APIs.
11. Git import/export passes interoperability and integrity tests through a
    separate gix adapter, while the same workflow remains usable with no Git
    repository present.
12. Required content and its safe retention are indelible properties of native
    views/publications rather than a graph walk and sidecar repair after the fact.

If the slice demonstrates only stable IDs stored beside ordinary Git commits,
it has not yet tested the central thesis; Gothab already has several variants
of that pattern.

## Constraints to preserve during exploration

- **Do not break ordinary Git adoption.** Git over HTTP/SSH remains a required
  compatibility path unless a later product decision explicitly changes it.
- **Do not make dual-write correctness implicit.** If Git and a native store
  coexist, one must be declared canonical per repository or operation, with
  durable projection checkpoints and recovery.
- **Do not let the projection become accidental authority.** A force-push or
  imported Git ref must enter through an explicit translation/publication
  operation before it changes native state.
- **Do not choose an encoding before lifecycle semantics.** Fast append and
  lookup benchmarks do not establish correct review, authorization, recovery,
  or deletion behavior.
- **Do not hold database transactions across storage or network work.** The
  existing publication and locking lessons continue to apply.
- **Treat every lock as optimization, not correctness.** Native writes must be
  idempotent, fenced, and recoverable under retries and expiry.
- **Keep content identity distinct from collaboration identity.** Stable
  change identity must not merely rename a hash of mutable projected content.
- **Design bounded work.** Normal publication should scale with the logical
  delta, not repeatedly traverse or rewrite the complete repository.
- **Keep immutable storage separate from publication coordination.** Do not make
  S3 pretend to be PostgreSQL or make PostgreSQL carry the repository object
  graph; adapters expose their real CAS, fencing, and durability contracts.
- **Keep physical packaging out of identity.** Loose versus segmented placement,
  compression, range offsets, replicas, and backend locations may change without
  changing any logical `ObjectId` or `ViewId`.
- **Make range lookup first-class but optional.** A backend lacking ranged reads
  remains correct through whole-object access; capable backends may use segment
  footers and paged indexes without changing repository semantics. Do not create
  segments so large that whole-object fallback is operationally unsafe.
- **Fail closed during collection.** Fresh loose objects get a minimum 24-hour
  grace period based on backend time, and ambiguous reachability, timestamps, or
  reader/catalog retention keeps data rather than deleting it.
- **Assume many parallel agents.** Ordering, causality, conflict, quotas, and
  provenance must be explicit rather than inferred from one developer's branch
  discipline.

## Suggested first investigation

Before selecting option C or D, produce a Git-assumption inventory organized by
consumer rather than crate. For each source-control fact, record:

| Question | Example answers |
|---|---|
| Current identity | ref name, commit OID, PR revision, push sequence, logical stack UUID |
| Current authority | bare repo, PostgreSQL row, object manifest, task output |
| Mutation | push, force-push, merge, rebase, mirror fetch, server commit |
| Consumers | review, CI, search, webhook, snapshot, billing, policy |
| Failure under richer semantics | identity loss, stale result, forced linearization, full graph walk |
| Candidate native primitive | change, revision, publication, dependency, operation, projection |

The inventory should include at least push/clone/fetch, pull requests and stacks,
review comments and approvals, merge gates, CI triggers and status, forks and
mirrors, branch/tag operations, server-generated commits, search, snapshots and
archives, ownership/GC, audit/webhooks, and repository deletion. Its purpose is
to identify the minimum coherent native boundary and the existing contracts
that a migration cannot silently change.

## Open questions for review

- Is the canonical unit a change, a snapshot, an operation, or a composition of
  all three?
- Are revisions immutable, and can one revision participate in more than one
  change or publication context?
- Is conflict state stored as content, an operation result, or a relation among
  revisions?
- Which server operations require total order, which require only causal order,
  and which can commute?
- Does a repository remain the storage and authorization boundary, or can
  changes/content span repositories and namespaces?
- What exactly does "publish" mean independently of updating a Git ref?
- How are Git-originated pushes translated when they lack stable native change
  IDs or contain merges that do not map cleanly to the native model?
- Can native history be projected to SHA-1 and SHA-256 Git repositories without
  making either hash part of native identity?
- Which metadata must travel to an offline client, and which remains
  server-authoritative?
- What can be garbage-collected when native revisions, Git projections,
  reviews, CI evidence, snapshots, and legal retention disagree about reachability?
- Is the protocol intended to be independently implementable and open, and if
  so, which semantics must be standardized rather than Gothab-specific?
- What result from a vertical prototype would falsify the server-native
  approach or show that the existing Git-plus-metadata model is sufficient?

## References

The gothab references below live in the gothab repository.

- [`crates/gothab-remote-helper/`](https://github.com/serialexp/gothab/tree/main/crates/gothab-remote-helper)
  — current `gothab://` and `gothab+ssh://` Git compatibility seam.
- [`large-file-storage.md`](https://github.com/serialexp/gothab/blob/main/docs/design/large-file-storage.md)
  — out-of-band blob transfer and the present remote-helper architecture.
- [`gitd-per-host-storage.md`](https://github.com/serialexp/gothab/blob/main/docs/design/done/gitd-per-host-storage.md)
  — server-side Git authority, routing, fencing, mutation, and recovery.
- [`repository-blob-ownership.md`](https://github.com/serialexp/gothab/blob/main/docs/design/repository-blob-ownership.md)
  — ownership manifests and content retention independent of one local ODB.
- [`stacked-pull-request-reconciliation.md`](https://github.com/serialexp/gothab/blob/main/docs/design/stacked-pull-request-reconciliation.md)
  — durable stack/review identity currently reconstructed around Git commits
  and refs.
- [Jujutsu architecture](https://docs.jj-vcs.dev/latest/technical/architecture/)
  — snapshot revisions, stable change identity, operation views, backend seams,
  and automatic descendant rewriting as useful requirements evidence rather
  than a storage format to copy blindly.
- [Zed DeltaDB](https://zed.dev/blog/sequoia-backs-zed) — adjacent
  operation-level provenance and collaborative-editing work; informative, but
  perfect character-level provenance is not a requirement for this effort.
- [ERSC: “An update on ERSC availability”](https://ersc.io/blog/ersc-availability)
  — public description of its substrate-first direction.
- [ERSC: “Martin joins ERSC”](https://ersc.io/blog/martin-joins-ersc) — public
  description of ERSC Storage and its private-beta timing.
