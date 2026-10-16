# Wheat Lessons from Scion — Design

Status: draft — review guidance and scope decisions landed; server-side publish
reconciliation, resumable content and publish, lazy content, sparse materialization,
derived-data indexes and deterministic simulation are not yet implemented
Owner: bart
Last updated: 2026-10-15

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move completed bullets from Outstanding to Done.

### Done

- [x] **Review guidance.** `CLAUDE.md` has a "Reviews" section: validate at trust
  boundaries, justify checks against trusted internal callers, and write performance
  contracts that count work rather than time.
- [x] **Decision — target size (2026-10-15).** Wheat targets repositories up to the
  size of TypeScript or Terraform. Larger monorepos are a non-goal, which defers
  lazy structure, sparse fetching and per-label heads (see "Scope").
- [x] **Decision — lazy granularity (2026-10-15).** Content only. Every client keeps
  the full structural closure.

- [x] **Publish contention benchmark.** `wheat-bench` case `publish_contention`:
  1 to 16 writers of unrelated changes against one server pinned to its own cores.
  First run (2026-10-15): 1.6 to 1.8 accepted publishes per second at 1 to 4
  writers, 0.7 at 16, with 7.7 attempts per publish and one change needing 281. The
  server was at most 2% busy; per-batch fsyncs and fetch-and-retry dominate (see
  "Measured contention").
- [x] **Staged writes for publish and fetch (2026-10-15).** The server stages a
  publish's uploaded objects in a `BatchObjectStore` and stores them once, after
  validation and before the head CAS; a refused publish stores nothing. Fetch and
  clone stage received and merged objects and store them once before the head
  moves. `packed_append` spans make the appends countable.
- [x] **Nagle off on both ends (2026-10-15).** Each request/reply round of a publish
  waited about 16 ms for a delayed ACK; with `TCP_NODELAY` a publish into a
  30-change repository went from 160 ms to 20 to 40 ms. With both changes, one
  writer reaches 7.1 accepted publishes per second (was 1.6); 16 writers reach 2.4
  (was 0.7), still at 13 attempts per publish.
- [x] **Server-side publish reconciliation (2026-10-15).** When the head moved past
  the publish's expected head, the server joins the proposed state into the current
  head with `union_envelopes`, the rules `wheat fetch` uses, and answers `stale` only
  where the join refuses. A per-repository commit lock makes concurrent publishes
  take turns from reading the head to its CAS. A forced publish must still name the
  current head.
- [x] **Client handling of a reconciled publish (2026-10-15).** The client no longer
  refuses to publish behind the remote. When the returned envelope is not the one it
  sent, it fetches the joined head's new objects over the same session, stores them
  in one write, and moves its head there; `wheat publish` reports `joined`. Every
  publish in `publish_contention` now takes one attempt, at every writer count.
- [x] **Decision — reconciled publish status (2026-10-15).** `ok`, with no wire
  change: the client tells a join from a fast-forward by comparing the returned
  envelope with the one it proposed.
- [x] **One append for content and objects together (2026-10-16).** A
  `RepositoryStore` is an object store with its content store, and its
  `put_publication` writes both: the packed store in one catalog append, the S3 store
  and separate stores content first. A `RepositoryStage` holds a publish's or fetch's
  objects and content, readable, until the server accepts the publish or the client
  moves its head, so a refused publish stores no content either. See "One append per
  publish".
- [x] **A cheaper append (2026-10-16).** The active segment is an append-only log of
  hash-chained frames: one write and one sync per append, with no journal and no
  head file. The locator runs `synchronous=NORMAL`. See "A cheaper append".
- [x] **Group commit (2026-10-16).** Publishes still join one at a time, but onto the
  last accepted publish before it is durable, and are stored and published in
  batches: one write and one head compare-and-swap per batch. See "Group commit".
- [x] **Packed appends and the catalog lock (2026-10-16).** Throughput stopped near
  10 per second at the packed store. All five fixes were tried cumulatively (see
  "Profile at 8 writers" and the sections after it): 8 writers now reach 41 to 47
  accepted publishes per second with one attempt each, and the server is about 5%
  busy there.

### Outstanding

- [ ] **Publish contention budget.** Contract rules on `publish_contention`: at least
  10 accepted publishes per second with 8 writers, and one attempt per publish at
  every writer count.
- [ ] **Client pushes the difference up front.** The client runs the server's walk
  from the last fetched head and sends the result, so a publish takes one round trip
  instead of one per tree level.
- [ ] **Defer unless the contention benchmark shows the server merge as the
  bottleneck — per-label heads.** Labels get their own CAS and changes are accepted
  without a head CAS.
- [ ] **Capability negotiation.** `ClientHello` gains a capability list and the
  server fills `ServerHello.capabilities`, so optional modes are switched on per
  connection.
- [ ] **Resumable — content offsets.** Per-blob start offsets in
  `ContentFetchRequest` and `ContentNeeded`; the server keeps partial uploads.
- [ ] **Resumable — publish.** Server-side `publish_key` storage, and remote
  publishes that keep received objects across a dropped connection.
- [ ] **Defer — resumable structural fetch.** A pending-fetch record in
  `remote.json` and pull-only retry or a have-hint on `FetchRequest`. Structural
  objects are small at the target size, so restarting the structural walk is
  acceptable; content resume covers the expensive part.
- [ ] **Decision — publish staging.** Store directly and seal on disconnect, or the
  designed staging with a TTL.
- [ ] **Lazy — content only.** Fetch the label view's `required_content`, fault in
  older content when a command needs it.
- [ ] **Decision — where content fault-in happens.** A faulting content-store
  wrapper or explicit per-command prefetch.
- [ ] **Decision — lazy client offline.** Fail or degrade when content is missing.
- [ ] **Decision — validation with missing content.** `validate_complete` reads
  every content blob; with lazy content it must skip, trust the server, or fetch
  everything.
- [ ] **Sparse materialization.** A path set in the workspace record; materialization
  and capture touch only those paths and treat the rest as unchanged. The store
  still holds the full structure.
- [ ] **Defer — lazy structure.** Stop the fetch walk early and fault in structural
  objects. Needs the store abstraction behind `LocalRepository`, a batched request
  by ID, and a partial-store answer for every `validate_envelope` caller.
- [ ] **Defer — sparse fetching.** Path-aware server walk and path prefixes on
  `FetchRequest`. Depends on lazy structure.
- [ ] **Derived data — one parent-recursive mechanism.** Change-ID index, path
  history and blame computed from parents' values, eagerly or lazily, in the store.
- [ ] **Open question — eager or lazy per index.** Which derived indexes are computed
  on publish and which on first use.
- [ ] **Derived data — server queries.** Expose the same queries over the native
  protocol, used when asked or when the client lacks the data (lazy content).
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
- Add the optional transfer modes that pay off at the target size (lazy content,
  sparse materialization, resumable content and publish) without making them the
  default.
- Give history queries (blame, path history, change-ID lookup) one mechanism that
  works locally and on the server.

## Non-goals

- Becoming Scion. Scion assumes always-online clients and no full repository
  anywhere. Wheat keeps full local clones as the default; offline work and local
  changes stay first-class.
- Monorepos beyond the size of TypeScript or Terraform. Google, Apple and Anthropic
  build their own systems for those.
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

### Before reconciliation

Changes are logically independent: a change depends only on its parents. Publication
is not. A publish sends one `RepositoryEnvelope`, and the server checks it against
the single `RepositoryHead {generation, envelope}` of the repository
(`crates/wheat-server/src/publish.rs`). The envelope reaches the `RepositoryState`,
which holds the label map and the retained roots of every change and revision.

So two agents that published revisions of unrelated changes from the same head
collided. One won the CAS. The other got `stale_head`, fetched (which unions the
remote state into its own), and published again, possibly losing the race again.
With a handful of writers this is invisible. With a swarm it means most publishes
cost a fetch and a retry, and the retry rate grows with the number of writers. This
is the global shared state Scion limits to one row per ref.

### The server reconciles (implemented 2026-10-15)

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
   answers `ok` with the new head. Publishes take turns on a per-repository commit
   lock from reading the head to the CAS; a CAS still lost (to another process on
   the same store) retries the join on the server, not across the network.

The client publishes a head it never saw in full. That is acceptable because the
join can only add: nothing the client published is dropped or changed.

When the returned envelope differs from the one it proposed, the client fetches the
joined head's objects over the same session, walking from its own proposed state,
and moves its head there. Its workspace head can only point at a state it holds in
full, so recording the returned head without those objects is not an option.

The join reuses the existing cost model: it compares the two roots and skips the
subtrees they share, so cost follows how far the states have drifted, not
repository size. The `publish_key` idempotency rules stay: a retry with the same key
returns the original receipt, including a reconciled one.

### Measured contention (2026-10-15)

`publish_contention` at full size, server on two physical cores, writers on the
other ten:

| writers | accepted/s | attempts per publish | worst change | p95 latency | server busy |
|---|---|---|---|---|---|
| 1 | 1.59 | 1.0 | 1 | 0.75 s | 1.5% |
| 2 | 1.80 | 1.5 | 21 | 0.74 s | 0.9% |
| 4 | 1.63 | 2.5 | 61 | 1.08 s | 0.7% |
| 8 | 1.21 | 4.5 | 139 | 1.32 s | 1.8% |
| 16 | 0.70 | 7.7 | 281 | 3.63 s | 1.5% |

The head CAS is not what is slow; the cost around it is. A publish into a
100-change repository took 0.35 s, of which 0.22 s was 99 fsyncs on the server: the
server walks the proposed state one tree level at a time, asks for 1 to 6 objects per
round, and commits each received batch durably (about 9 fsyncs per batch). A fetch
that keeps local work did 111 fsyncs, 0.24 s of 0.29 s, in its radix updates. Every
refusal costs such a fetch and another publish, and with nothing ordering the
writers an unlucky one keeps losing.

Hence the order of work: one durable commit per operation, then server
reconciliation, then a client-side push of the difference to cut round trips.

With staged writes, `TCP_NODELAY` and reconciliation (same machine and pinning):

| writers | accepted/s | attempts per publish | worst change | p95 latency | server busy |
|---|---|---|---|---|---|
| 1 | 6.24 | 1.0 | 1 | 0.09 s | 0.9% |
| 2 | 6.43 | 1.0 | 1 | 0.38 s | 0.8% |
| 4 | 8.73 | 1.0 | 1 | 0.41 s | 2.3% |
| 8 | 8.63 | 1.0 | 1 | 1.30 s | 4.1% |
| 16 | 6.87 | 1.0 | 1 | 3.21 s | 10.2% |

Almost every publish from 2 writers on is joined.

#### Profile at 8 writers (2026-10-15, after content maps)

`WHEAT_TRACE` on the bench's in-process server, 160 publishes at about 9.7 per
second. Each publish holds the commit lock about 96 ms:

| step under the commit lock | ms per publish |
|---|---|
| `publish_store`: one packed append of the staged objects | 50 |
| `publish_join`: `union_envelopes`, of which `union_states` 4 to 7 | 26 to 29 |
| `publish_validate` | 10 to 13 |
| `publish_cas`: head file write | 9 to 10 |

The packed store is the bottleneck, not the join's own work:

- A publish makes two packed appends (content outside the lock, objects inside), at
  about 46 ms each. Some append is running for 89% of the wall time, so appends
  alone cap throughput near 10.7 per second.
- Every packed read and append takes one `std::sync::Mutex<PackedCatalog>`. An append
  holds it through all its fsyncs, so reads queue behind appends:
  `packed_catalog_wait` totals 165 ms per publish, of which 28 ms inside the join and
  10 ms inside validation (both under the commit lock) and 46 ms in the post-join
  `fetch_push`. The join's own work is about 2 ms with one writer. The lock and the
  fsyncs also block the runtime's worker threads.
- One append of 46 ms: journal write 10, segment write and sync 6, head write 8,
  journal removal 4, SQLite index transaction 17 (`journal_mode=DELETE`,
  `synchronous=FULL`). The segment data itself is the smallest part.

#### Reads beside an append (2026-10-16)

The fixes from that profile are being tried cumulatively, measuring after each.
First: a durable append does its I/O without the catalog mutex. It plans under the
lock, then runs the journal, segment write, head, journal removal and locator rows
holding only a writer lock, on its own file handle and its own SQLite connection,
then takes the catalog again to install the result. Reads go on meanwhile and see
only published records. The locator moved to `journal_mode=WAL` so the reading
connection is not blocked by the appending one's commit.

| writers | accepted/s before | after (two runs) |
|---|---|---|
| 1 | 7.54 | 8.80, 8.53 |
| 2 | 6.72 | 5.62, 5.82 |
| 4 | 8.43 | 10.66, 9.47 |
| 8 | 9.24 | 13.67, 13.32 |
| 16 | 9.13 | 12.67, 11.83 |

Every count still takes one attempt per publish. Catalog waits fell from 165 ms to
about 2 ms per publish. The locator transaction fell from 17 ms to about 5, so an
append takes 33 to 37 ms. Appends are now the ceiling: at 8 and 16 writers one is
running 96% of the wall time, and a publish still makes two.

The 2-writer figure is noise, not a regression: earlier runs at that level gave 5.05
to 6.72, and in the traces the server's time per 2-writer publish fell from 115 to
94 ms. At 1 and 2 writers the server is idle about 75 ms between publishes, so the
clients set the pace there.

#### One append per publish (2026-10-16)

Second: content is staged with the objects and both are stored in one packed append
at `publish_store`, inside the commit lock. The client's fetch does the same.

| writers | accepted/s after reads beside an append | after (two runs) |
|---|---|---|
| 1 | 8.80, 8.53 | 8.45, 9.29 |
| 2 | 5.62, 5.82 | 8.18, 8.16 |
| 4 | 10.66, 9.47 | 12.69, 13.47 |
| 8 | 13.67, 13.32 | 18.72, 18.81 |
| 16 | 12.67, 11.83 | 17.92, 18.42 |

Still one attempt per publish at every count. Each publish now holds the commit lock
about 52 ms at 8 writers: the append 38 (journal 9, segment 6 to 8, head 10,
journal removal 5, locator 5.5), the head CAS 10 to 11, validation and join 3. That
caps throughput near 19 per second, which is what the benchmark reaches; at 16
writers a publish waits 520 ms for the lock.

#### A cheaper append (2026-10-16)

Third: the append itself. A V1 segment cannot grow by appending, because its
payloads must be contiguous and its index sits after them; a fresh index per
append would make the file quadratic in its record count. Sealing already
rebuilds the records into a V1 segment, so the active segment became a log:

- A header (`ActiveLogHeaderV1`) holding the generation it starts at.
- One frame per append: a frame header (`ActiveLogFrameHeaderV1`: generation,
  payload and index lengths, and a digest of those fields chained to the previous
  frame), the payloads, a `SegmentIndexPageV1` with the frame's entries, and a
  trailer (`ActiveLogFrameTrailerV1`) whose digest chains over the whole frame.
- The newest verified frame is the published state; its chain digest is the state's
  ID. There is no head file and no journal: an append writes one frame and syncs once,
  and a failed write or sync truncates the frame away.
- Opening walks the frames and verifies every chain digest, hashing the whole log
  as opening did before. A frame cut short at the end of the file, or ending in
  zeroed bytes, is a torn append and is truncated. A frame that fails its digest with
  another frame after it refuses to open.

The SQLite locator moved to `synchronous=NORMAL`. In WAL mode a crash can lose the
last locator transactions but not corrupt the database, and the locator is rebuilt
from the segments when it does not match them.

| writers | accepted/s after one append per publish | after (two runs, plus the saved run) |
|---|---|---|
| 1 | 8.45, 9.29 | 15.72, 15.34, 11.48 |
| 2 | 8.18, 8.16 | 12.56, 14.37, 14.17 |
| 4 | 12.69, 13.47 | 22.61, 23.91, 21.64 |
| 8 | 18.72, 18.81 | 40.19, 45.21, 43.35 |
| 16 | 17.92, 18.42 | 49.61, 52.95, 53.57 |

Still one attempt per publish at every count. In the trace at 8 writers an append
takes about 4.4 ms (segment write and sync 3.7, locator 0.7), down from 38. The head
CAS, an atomic write of the head file at 5 to 9 ms, is now the largest step under
the commit lock, and at 16 writers a publish waits about 220 ms for it.

Trade-offs:

- Each frame adds 96 bytes of header and trailer and its own index page, so the
  active segment grows faster than it did. It is still sealed by size and count.
- A damaged frame in the middle of the log stops the store from opening rather than
  dropping the frames after it.
- Stores written before this change have an active segment in the old format and do
  not open. No released store uses it.

#### Group commit (2026-10-16)

Fourth: publishes are made durable in batches (`wheat-server` `commit.rs`). A publish
still joins and validates under a lock, one at a time, but onto the tentative head:
the last accepted publish's envelope, durable or not. It then hands its stage's
objects and content to the next batch and waits. A committer task stores a whole
batch in one `put_publication`, seals, and moves the head once with one
compare-and-swap. While it writes, the publishes after it join onto its head,
reading its objects from memory (`PendingPublications::over`), and form the next
batch.

- Only the join and validation read the pending objects. A publish still receiving
  checks presence against the durable store, so it never leaves out an object or
  blob that only a batch which might yet fail holds.
- Every publish in a batch is told the batch's head, which may include publishes
  accepted after it; to the client that is an ordinary join.
- A failed batch fails the batch joined onto it, as stale, so those clients publish
  again from the durable head.
- If the durable head is not the one a batch was built on (another process sharing
  the store published, or an earlier batch was re-joined for that reason), the
  committer joins the batch with it before the swap, as a publish used to. A batch
  holding a forced publish is sent back as stale instead.
- The committer runs in a task of its own, so a client that goes away does not stop
  the others' batches. If it panics, its publishes fail and the next publish starts
  a new one.

| writers | accepted/s after a cheaper append | after (two runs, plus the saved run) |
|---|---|---|
| 1 | 15.72, 15.34, 11.48 | 13.64, 16.81, 16.81 |
| 2 | 12.56, 14.37, 14.17 | 13.98, 12.18, 14.43 |
| 4 | 22.61, 23.91, 21.64 | 25.90, 25.17, 28.43 |
| 8 | 40.19, 45.21, 43.35 | 34.90, 41.57, 42.15 |
| 16 | 49.61, 52.95, 53.57 | 65.26, 62.75, 69.80 |

Still one attempt per publish at every count. Only 16 writers gain, because only
there was the commit lock full. In the trace at 8 writers 160 publishes took 131
batches and the committer was busy about half the time. The server spends about
20 ms handling a publish, but each writer starts one only about every 170 ms, so at
8 writers the time outside the server's publish handling sets the pace. At 16
writers a batch averaged 2.2 publishes, again with the committer busy about half the
time.

#### The committer off the runtime (2026-10-16)

Fifth: the committer runs on Tokio's blocking pool (`spawn_blocking` driving it with
`Handle::block_on`), so the batch write and the head's syncs, about 8 to 12 ms per
batch, no longer hold a runtime worker that connections need. The rest of the
server's disk work is reads, many per publish and each short. Moving each one to the
blocking pool would cost a handoff per read (see the TODO item on blocking disk
work), so they stay where they are.

| writers | accepted/s after group commit | after (two runs, plus the saved run) | latency p50, ms (before, after) |
|---|---|---|---|
| 1 | 13.64, 16.81, 16.81 | 16.78, 15.23, 16.20 | 18, 18 |
| 2 | 13.98, 12.18, 14.43 | 13.96, 13.82, 11.77 | 54, 54 |
| 4 | 25.90, 25.17, 28.43 | 24.89, 29.31, 28.17 | 58, 59 |
| 8 | 34.90, 41.57, 42.15 | 44.68, 41.25, 47.08 | 83, 76 |
| 16 | 65.26, 62.75, 69.80 | 70.75, 70.63, 67.19 | 121, 118 |

Latencies are from the saved runs.

Still one attempt per publish. The gains are within the runs' spread, as the profile
predicted: the server is about 5% busy at 8 writers and 16% at 16.

Where a writer's time goes now: at 1 writer a publish takes 18 ms, but from 2 writers
on, every publish is joined and the median jumps to about 55 ms. The server's join and
push add only about 1 ms, so the rest is likely the client taking a joined head. That
is not yet profiled.


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

### Scope (decided 2026-10-15)

Wheat targets repositories up to the size of TypeScript or Terraform (TypeScript has
about 31,600 commits on main). At that size structural objects are small next to file
content, and most of a clone is old content that agents and CI never read. So:

- **Lazy content only (decision 1).** Every client keeps the full structural
  closure; only content outside the current label view arrives on demand. Most rows
  of the dependency table stay as they are. Decisions 2 to 4 still apply, narrowed to
  content.
- **Sparse materialization, not sparse fetching (decision 5).** A workspace may
  write only some paths to disk, which needs the existing path filters in
  materialization and capture. Per workspace fits that, as `wheat-local-cli.md`
  sketches. Sparse fetching depends on lazy structure and is deferred with it.
- **Resume content and publish, not the structural fetch (decision 6).** Restarting
  a structural walk is cheap at the target size; restarting a large blob is not.
  Decision 6 is deferred with the structural resume.
- **Lazy structure and per-label heads are deferred.** They stay in the checklist
  as "Defer" so they are not forgotten if the target changes.

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
  asked to, or by default when it does not hold the data. With lazy content that is
  mostly blame over history the client has not fetched; Gothab's web UI needs the
  same queries on the server regardless.
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
2. Server-side reconciliation and client handling of a reconciled publish (done);
   add the benchmark's contract rule.
3. Capability negotiation, then resumable content and publish, then lazy content
   and sparse materialization, each after its decisions above.
4. The parent-recursive derived-data mechanism, starting with the change-ID index.
5. Server queries over the native protocol.
6. Deterministic simulation, after the Gothab integration.

## Open questions for review

- Which derived indexes are computed eagerly on publish and which lazily?

## References

- [Redesigning source control][scion], Anthropic engineering blog, 2026-10-13.
- `docs/design/wheat-native-protocol.md`: fetch, publication, head management.
- `docs/design/server-native-source-control-store.md`: repository head, persistent
  trees, product boundary.
- `docs/design/wheat-local-cli.md`: sparse paths and history utilities.

[scion]: https://www.anthropic.com/engineering/redesigning-source-control
