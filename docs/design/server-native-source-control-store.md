# Wheat — Server-Native Source Control

Status: partial — bounded Git-free walking skeleton underway; core change/revision/application semantics and a local prototype CLI exist, while the store format, protocol, Git adapter, and product commitment remain undecided
Owner: bart
Last updated: 2026-09-03

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

### Outstanding

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
- [ ] **Decision gate — compatibility contract.** Decide what must round-trip
  through ordinary Git, what may be a lossy projection, and whether
  `git-remote-gothab` initially delegates to Git or materializes Git objects
  from a native service.
- [ ] **Phase 1 — qualify the native model and store prototype.** Replace the
  walking skeleton's deliberately simple whole-file transformations and YAML
  state snapshot with content manifests and reachability, transactional
  metadata/content publication, dependency validation, and property tests. The
  prototype now retains verified content bytes, but does not yet prove ownership,
  recovery, or safe reclamation; Git and gix remain outside the core.
- [ ] **Phase 1 — complete the reference CLI and local workflow.** Extend the
  current local CLI with workspace capture, composition, explicit publication,
  richer conflict resolution, and black-box ergonomics tests; the current
  exact-edit commands exercise the model but are not a daily-use client.
- [ ] **Phase 1 — complete the conflict and concurrency corpus.** Extend the
  current chain/branch/join/conflict/CAS tests to concurrent human/agent
  mutations, independent simultaneous conflicts, retries, dependency movement,
  and property-tested deterministic outcomes without a mutable Git ref as truth.
- [ ] **Phase 1 — lifecycle and content-retention prototype.** Exercise
  publication, crash recovery, content manifests, shared-content retention,
  snapshot/restore, and deletion as native store invariants rather than adding
  them after the format is fixed.
- [ ] **Defer until publication targets exist — native target invariants.** Make
  rules such as “`main` never points to a view with unresolved conflicts” part
  of the repository's fenced target-advance operation, while allowing other
  targets to publish conflicted views; the forge presents and administers these
  policies but is not their sole enforcement boundary.
- [ ] **Phase 2 — physical store qualification.** Benchmark and refine object
  encoding, indexes, manifests, deltas, sharding, replication, compaction, and
  garbage collection against deep stacks and wide agent fan-out.
- [ ] **Phase 2 — native protocol and remote CLI.** Specify a resumable,
  versioned client/server protocol and run the same workflow corpus through the
  reference CLI against an ephemeral repository daemon.
- [ ] **Phase 3 — separate gix compatibility adapter.** Import Git into native
  objects and project selected native views to disposable Git repositories or
  synthesized packs without allowing Git identities to become authoritative.
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
and can retrieve them after reopening. This is not yet intrinsic ownership: no
view manifest pins its closure, no recovery reconciles metadata with content, and
there is no garbage collector or working-copy materialization.

### Crate and dependency boundaries

The VCS and CLI are named **Wheat**. Crate names use the `wheat-*` prefix;
the installed reference-client binary is `wheat`. Dependency direction is the
contract:

```text
wheat-core
  pure identities, values, algorithms, conflict semantics, and store traits
  no gix, PostgreSQL, S3, Axum, gitd, or existing PR-model dependency

wheat-store
  native persistent repository and content-manifest format
  depends on wheat-core

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
  depends on wheat-core and gix

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

Publication targets should ultimately be native, policy-constrained repository
objects rather than untyped forge-owned branch labels. For example, `main` may
carry a repository invariant that it can point only to a complete view with zero
unresolved conflicts, while feature and agent targets remain free to publish
conflicted views for browsing, review, assignment, and collaborative resolution.
Every mutation path—including the native CLI/API, merge queue, recovery, and Git
compatibility adapter—must pass through the same fenced target-advance operation.
The active policy on the current target governs an update; a proposed view cannot
weaken its own protection merely by changing a versioned policy file. This is a
later publication-model requirement, not part of the current content-retention
slice.

A revision may preserve both the transformation that can be reapplied and its
materialized result:

```text
Revision {
    change_id,
    predecessors,
    authored_against,
    transformation,
    resulting_tree,
}

apply(transformation, authored_against.tree) == resulting_tree
```

This is a design hypothesis, not a settled encoding. Transformations may be
file/tree edits, merge terms, or another representation found through the
workflow corpus. They are not promised to commute or apply cleanly to every
base. Application to a different target returns an exact clean result or a
structured conflict.

### Operations are not logical changes

An observed filesystem mutation and a reviewable logical change have different
cardinality. One agent task can contain many edit, write, formatter, generator,
and shell-process mutations. Conversely, one broad process invocation may
change files for several independent concerns that are later split.

The default client behavior should treat tool or filesystem mutations as
immutable causal operations that amend the current logical change:

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
```

`ContentId` is native and algorithm-tagged. Git SHA-1/SHA-256 blob IDs are
projection identities computed by the compatibility adapter, not native content
identity.

The repository format owns **what** content a view and all retained native
objects require. The object router owns **where** durable and cached copies live.
Bucket, backend, and gitd-host placement must not enter native repository
identity.

Every published view must commit to an immutable, incrementally transitionable
content manifest. Required content includes at least visible tree closure,
retained revisions, unresolved conflict terms, snapshots/archives under policy,
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

### Git compatibility and repository daemon

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

- `crates/gothab-remote-helper/` — current `gothab://` and
  `gothab+ssh://` Git compatibility seam.
- [`large-file-storage.md`](large-file-storage.md) — out-of-band blob transfer
  and the present remote-helper architecture.
- [`gitd-per-host-storage.md`](gitd-per-host-storage.md) — server-side Git
  authority, routing, fencing, mutation, and recovery.
- [`repository-blob-ownership.md`](repository-blob-ownership.md) — ownership
  manifests and content retention independent of one local ODB.
- [`stacked-pull-request-reconciliation.md`](stacked-pull-request-reconciliation.md)
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
