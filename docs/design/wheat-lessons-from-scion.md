# Wheat Lessons from Scion — Design

Status: draft — review guidance landed; server-side publish reconciliation,
optional sparse/on-demand/resumable transfer, derived-data indexes and deterministic
simulation are not yet implemented
Owner: bart
Last updated: 2026-10-15

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move completed bullets from Outstanding to Done.

### Done

- [x] **Review guidance.** `CLAUDE.md` has a "Reviews" section: validate at trust
  boundaries, justify checks against trusted internal callers, and write performance
  contracts that count work rather than time.

### Outstanding

- [ ] **Publish contention benchmark.** A `wheat-bench` case with N concurrent
  publishers of unrelated changes against one server, counting stale answers and
  retries per accepted publish. Smoke size in `just test`, contract rule once the
  reconciliation below lands.
- [ ] **Server-side publish reconciliation.** On a stale expected head, the server
  merges the proposed state into its current head with the same rules `wheat fetch`
  uses, and refuses only real collisions.
- [ ] **Client handling of a reconciled publish.** The client records the returned
  head without a fetch, and its next fetch walks from that head.
- [ ] **Decision — reconciliation versus per-label heads.** Decide whether server
  reconciliation is enough or whether labels get their own CAS later.
- [ ] **Open question — reconciled publish status.** Answer `ok`, or a distinct
  status so clients and logs can tell a fast-forward from a join.
- [ ] **Capability negotiation.** `ClientHello` gains a capability list and the
  server fills `ServerHello.capabilities`, so optional modes are switched on per
  connection.
- [ ] **Resumable — fetch.** A pending-fetch record in `remote.json` and a way to
  skip what already arrived (decision below).
- [ ] **Resumable — content offsets.** Per-blob start offsets in
  `ContentFetchRequest` and `ContentNeeded`; the server keeps partial uploads.
- [ ] **Resumable — publish.** Server-side `publish_key` storage, and remote
  publishes that keep received objects across a dropped connection.
- [ ] **Decision — fetch resume.** Pull-only retry or a have-hint on `FetchRequest`.
- [ ] **Decision — publish staging.** Store directly and seal on disconnect, or the
  designed staging with a TTL.
- [ ] **Lazy — content only.** Fetch the label view's `required_content`, fault in
  older content when a command needs it.
- [ ] **Lazy — structural.** Stop the fetch walk early and fault in structural
  objects, which needs the store abstraction and a batched request by ID.
- [ ] **Decision — lazy granularity.** Content only, or content and structure.
- [ ] **Decision — where fault-in happens.** A faulting store wrapper or explicit
  per-command prefetch.
- [ ] **Decision — lazy client offline.** Fail or degrade.
- [ ] **Decision — validation in a partial store.** Validate what is held, trust the
  server's head, or keep full validation as fetch-everything.
- [ ] **Sparse paths.** Path set in the workspace record, filtered materialization
  and capture, path-aware server walk, path prefixes on `FetchRequest`.
- [ ] **Decision — sparse scope.** Per workspace or per store.
- [ ] **Derived data — one parent-recursive mechanism.** Change-ID index, path
  history and blame computed from parents' values, eagerly or lazily, in the store.
- [ ] **Open question — eager or lazy per index.** Which derived indexes are computed
  on publish and which on first use.
- [ ] **Derived data — server queries.** Expose the same queries over the native
  protocol, used when asked or when the client lacks the data (sparse clone).
- [ ] **Defer until Wheat runs inside Gothab — deterministic simulation.** A
  simulated client/server harness with dropped connections, interrupted transfers
  and many concurrent publishers, driving the real CLI code paths.

## Why this exists

On 2026-10-13 Anthropic described Scion, the source-control system that replaced git
for their monorepo ([Redesigning source control][scion]). Merges went from about
2,500 a day in May 2026 to over 30,000 in September, mostly made by agents. The
history is near 100 GB and CI does about 25 million checkouts a day.

Scion was built for the same pressure Wheat exists for: many agents writing
concurrently against shared state that was designed for a few humans. Most of its
conclusions agree with decisions Wheat already made. A few point at places where
Wheat would hit the same wall. This doc records which is which, so the follow-up
work has its reasoning attached.

## Goals

- Record what Scion does that bears on Wheat, and what Wheat takes from it.
- Fix publish contention between unrelated changes before the native schema is
  locked.
- Keep sparse, on-demand and resumable transfer possible as optional modes without
  making them the default.
- Give history queries (blame, path history, change-ID lookup) one mechanism that
  works locally and on the server.

## Non-goals

- Becoming Scion. Scion assumes always-online clients and no full repository
  anywhere. Wheat keeps full local clones as the default; offline work and local
  changes stay first-class.
- A Spanner (or similar managed database) backend. Scion's storage choice fits
  Anthropic's budget. Wheat's backends stay configurable and nobody plans this one.
- Removing packed segments or repacking. Wheat's segments exist because per-object
  requests to object storage are too expensive; that argument stands.

## What Scion does

- **Clients fetch on demand.** No fully materialized repository exists anywhere,
  not even on the server.
- **The server does the heavy work.** History, blame, commits, merges and rebases
  happen on the server, and every query the client makes is also a public API, so
  many jobs need no checkout at all.
- **Changesets list changed files against the first parent**, with pointers to
  content-addressed blobs. Accepting one takes no locks, no packfiles and no ref
  update; cost follows the size of the change.
- **Refs are rare.** Moving one is a separate compare-and-swap on one database row.
  Refs other than main and release branches are discouraged, because global shared
  state is a scaling hazard.
- **Derived data** (blame, path history, commit-graph skip lists, later copy
  detection and path search) comes from one recursive mechanism: each value is
  computed from its parents' values, eagerly or lazily.
- **Fetch never advertises what the client has.** The client walks down from the
  root tree asking for what it lacks. Requests are resumable and there are no large
  pack responses, so sparse and virtual checkouts follow naturally.
- **Three cache tiers** answer identical requests, each with over 99% hit rate.
- **No merge queue with a global lock.** Hundreds of changes land during one test
  run, so testing moved to after merge and before deploy, and Claude resolves
  conflicting changes.
- **Process.** Agents sometimes copied patterns that should never have been allowed,
  such as scanning the commit log linearly to find merge times. Adversarial review
  sometimes added needless validation against trusted internal services, which had
  to be rolled back. The team relies on hermetic simulations of every non-trivial
  interaction plus fuzzing.

## Where Wheat already agrees

- **Conflicts are data.** Wheat stores a conflict as a durable application result
  and checks integration later, the same conclusion Scion reached when it dropped
  the locking merge queue.
- **Cost follows the change.** Publication validation is bounded to the transition
  (`wheat-native-protocol.md`, "Bounded delta validation"), not the repository.
- **Edits are relative.** Guarded edits describe a change against its target, close
  to Scion's first-parent changesets, while views keep persistent trees for fast
  reads.
- **Content-addressed, immutable objects** let any server or cache hold them without
  coordination.

## Publish contention on the repository head

### Today

Changes are logically independent: a change depends only on its parents. Publication
is not. A publish sends one `RepositoryEnvelope`, and the server checks it against
the single `RepositoryHead {generation, envelope}` of the repository
(`crates/wheat-server/src/publish.rs`, the expected-head checks at the start of the
publish and the CAS at the commit). The envelope reaches the `RepositoryState`,
which holds the label map and the retained roots of every change and revision.

So two agents that publish revisions of unrelated changes from the same head
collide. One wins the CAS. The other gets `stale_head`, fetches (which unions the
remote state into its own), and publishes again, possibly losing the race again.
With a handful of writers this is invisible. With a swarm it means most publishes
cost a fetch and a retry, and the retry rate grows with the number of writers. This
is the global shared state Scion limits to one row per ref.

### Proposal: the server reconciles

Every part of a repository state only grows, and `wheat fetch` already knows how to
join two states: the union of both, with a three-way merge of labels against the
last common envelope (`wheat-native-protocol.md`, under "Fetch"). The server can run
that same join instead of answering `stale`:

1. The publish names its expected head, as now.
2. If the server's current head differs, the server joins the proposed state with
   its current head, using the expected head's labels as the merge base.
3. The join refuses in the same cases fetch stops on: a label both sides moved
   differently, a change recorded with a different change object on each side, or
   states from different base views. Those still answer `stale`, with a reason.
4. Otherwise the server writes the joined state, advances the head by CAS, and
   answers `ok` with the new head. A CAS lost during this step retries the join on
   the server, not across the network.

The client then records the returned head. Its own state is part of that head, so it
needs no fetch to stay consistent; other writers' objects arrive on its next fetch,
which walks from the new head.

The client publishes a head it never saw in full. That is acceptable because the
join can only add: nothing the client published is dropped or changed.

The join reuses the existing cost model: it compares the two roots and skips the
subtrees they share, so cost follows how far the states have drifted, not
repository size. The `publish_key` idempotency rules stay: a retry with the same key
returns the original receipt, including a reconciled one.

### Alternative: per-label heads

Labels could get their own CAS, and changes and revisions could be accepted into
the immutable store with no head CAS at all, closer to Scion's design. That removes
contention entirely instead of moving it to the server, but changes the shape of
`RepositoryState` and the publication contract. Server reconciliation does not rule
this out later; the contention benchmark should show whether it is ever needed.

## Optional transfer modes

Three features that most repositories never need and that should be switchable:

- **On-demand fetch.** The client fetches structural objects or content only when a
  command needs them.
- **Sparse paths.** A clone or workspace fetches and materializes only some paths
  (`sparse_paths` is already sketched as deferred in `wheat-local-cli.md`).
- **Resumable transfers.** An interrupted fetch or publish continues without
  re-sending what already arrived.

The obstacle is an invariant, not a flag: the client moves its head only after the
structural walk and the content fetch finish, "so its store always holds the closure
of its last fetched envelope" (`wheat-native-protocol.md`). Local commands, the
client side of `StructuralDelta`, and fetch's union with local work may depend on
that. Each dependency needs one of: fault the object in from the remote, ask the
server, or refuse in sparse mode.

### What exists today (surveyed 2026-10-15)

- **Missing objects are hard errors** everywhere except `StructuralDelta`, which
  parks a walk on a missing new-side object and treats a missing old-side object as
  "no counterpart".
- **A fallback seam exists but is not reachable.** `ImmutableObjectStore` is an async
  trait with wrappers already (`BatchObjectStore`, `CachedObjects`), so a faulting
  wrapper fits. But `LocalRepository` holds the concrete `PackedObjectStore` and
  `ContentStore`, and `ContentStore` is synchronous, so fetching over the network
  from inside it means a network call from synchronous code under `block_on`.
- **Objects by ID** are only served inside a fetch, after `FetchPushEnd`. A
  `FetchRequest` with `want == since` gives an empty push followed by `ObjectWant`
  rounds without a wire change. `ContentFetchRequest` already stands alone.
- **Path filtering exists** (`TreePathFilter`, the diff cursor's `restricted_to`, the
  workspace scan's filter), but materialization walks the whole tree and the server's
  tree walk does not track paths.
- **Partial resume exists at object level.** Fetched content already held is
  skipped and stored in 64 MiB batches; publish content the server has is skipped;
  the local packed store writes batches to disk as they arrive. A retried fetch
  still receives the whole push, a partly received chunked blob is lost,
  `RemotePackedStore` only seals at commit so a dropped publish loses its objects,
  and the server ignores `publish_key`.
- **`ServerHello.capabilities`** exists but is sent empty, and `ClientHello` has no
  capability list. `Credit` and `ContentRedirect` exist in the schema unused.

### What depends on the full closure

| Area | Needs |
|---|---|
| `validate_envelope` → `validate_complete` (label and frontier commands, push, materialization recovery, repack, snapshot, `wheat validate`, publication prepare, rewrite) | A cheaper check or a refusal on a partial store. This full-history scan is already a cost problem today (`TODO.md`, "Read-only `wheat` commands cost a full validation"). |
| Client side of `StructuralDelta` | A restricted set of stages. A missing old side only makes it fetch more. |
| Fetch union with local work | Fault in the remote radix nodes where the sides differ |
| `add` / `change` / `stack` radix updates and fresh-content checks | Fault in along the touched paths |
| Integrate and squash edit replay | Fault in content; in sparse mode refuse or fault in outside paths |
| Materialization and workspace scan content | Fault in or prefetch the view's content |
| Git export | Fault in history content |
| Snapshot inventory, ownership proofs, metadata repack, store analysis | Refuse, or model the store as partial |
| Server-side publish walk | Unaffected: it only asks for objects the client wrote |

### Sizes

- **Resumable: medium.** Object-level skipping mostly exists; what is left is
  chunk offsets, the pending-fetch record, `publish_key`, and sealing or staging for
  remote publishes.
- **Sparse: medium to large.** The filters exist, but materialization, capture,
  integrate, the server walk and the wire all change. Sparse structure depends on
  lazy structure.
- **Lazy: large for structure, medium for content only.** Structure needs the store
  abstraction behind `LocalRepository`, replacing the `validate_envelope` call sites,
  and a batched fault-in request so a tree descent is not one round trip per node.

### Decisions to make first

1. **Lazy granularity.** Content only keeps the structural closure intact and is
   smaller. Content and structure is the full feature and touches every row above.
2. **Where fault-in happens.** A transparent store wrapper is simple for callers but
   puts network calls in synchronous code and needs a session per command. Explicit
   prefetch per command is predictable but every command must declare what it needs.
3. **A lazy client offline.** Fail, or degrade to what it holds.
4. **Validation in a partial store.** Validate only what is held, trust the server's
   head, or keep full validation as fetch-everything.
5. **Sparse scope.** Per workspace (as `wheat-local-cli.md` sketches) means a shared
   store holds the union of its workspaces. Per store is simpler but coarser.
6. **Fetch resume.** Pull-only retry needs no wire change but goes one request at a
   time. A have-hint on `FetchRequest` keeps streaming but changes the wire.
7. **Publish staging.** Store directly and seal on disconnect, as the local store
   effectively does, or the designed staging area with a TTL.

Two things in `wheat-native-protocol.md` disagree with the code: "Resumable large
transfers" is listed as resolved with resume from the last received chunk, which
does not exist, and `publish_key` idempotency is described but not implemented. Both
are tracked in `TODO.md`.

## Derived data and server queries

Blame, path history and the change-ID index are deferred
(`wheat-local-cli.md`, "Defer — advanced history utilities";
`server-native-source-control-store.md`, "Change-ID indexing ... remain").
Scion builds all of them with one mechanism: a value for a revision is computed
from the values of its parents, stored, and computed eagerly or on first use. New
index types are cheap to add and internals can change without client changes.

For Wheat:

- The mechanism lives in the store and runs locally by default, because local
  changes exist that no server has seen.
- The same queries are exposed over the native protocol. A client uses them when
  asked to, or by default when it does not hold the data (a sparse or on-demand
  clone).
- Each index type gets a work-counting contract (values computed per query) in
  `wheat-bench`, so a linear scan of history fails a test.

## Process lessons

- **Review balance.** Captured in `CLAUDE.md` under "Reviews".
- **Contracts count work.** Also in "Reviews". Scion's linear merge-time scan is the
  example: a count of objects read per query catches it on any machine.
- **Deterministic simulation.** Valuable, but deferred until Wheat runs inside
  Gothab, so the harness drives the server that is actually used.

## Positioning

Scion's authors say it only pays off for enormous monorepos or agent swarms, expect
the problem to become common within 6 to 12 months, and ask for richer server-side
requests standardized across forges. That is the gap Wheat aims at, and
`wheat-native-protocol.md` already settles the product boundary as an open protocol
rather than an internal substrate.

## Phasing

1. Contention benchmark, so the problem has a number.
2. Server-side reconciliation and client handling of a reconciled publish; add the
   benchmark's contract rule.
3. Capability negotiation, then resumable transfers, then lazy content, then lazy
   structure and sparse paths, each after its decisions above.
4. The parent-recursive derived-data mechanism, starting with the change-ID index.
5. Server queries over the native protocol.
6. Deterministic simulation, after the Gothab integration.

## Open questions for review

- Is server reconciliation enough, or do labels need their own CAS?
- Should a reconciled publish answer `ok` or a distinct status, so clients and logs
  can tell a fast-forward from a join?
- Which derived indexes are computed eagerly on publish and which lazily?

## References

- [Redesigning source control][scion], Anthropic engineering blog, 2026-10-13.
- `docs/design/wheat-native-protocol.md`: fetch, publication, head management.
- `docs/design/server-native-source-control-store.md`: repository head, persistent
  trees, product boundary.
- `docs/design/wheat-local-cli.md`: sparse paths and history utilities.

[scion]: https://www.anthropic.com/engineering/redesigning-source-control
