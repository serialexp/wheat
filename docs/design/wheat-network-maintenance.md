# Wheat network maintenance: moving networks and collecting garbage — Design

Status: draft, not yet implemented; open questions below await Bart's review.
Owner: bart
Last updated: 2026-10-27

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they land
and move them from Outstanding to Done.

### Done

- [x] **Decision — one design for move and GC (2026-10-27).** Bart chose one design
  covering moving a network, adopting a standalone repository and garbage collection
  before any of it is built.
- [x] **Decision — run inside the server (2026-10-27).** Import and GC run in the
  serving process, which holds the content store's lock, rather than with the server
  stopped.

### Outstanding

- [ ] **Open question 1 — the admin surface.** See [Admin surface](#admin-surface).
- [ ] **Open question 2 — how a bundle travels.** See [Moving a network](#moving-a-network).
- [ ] **Open question 3 — GC and concurrent publishes.** See [Concurrency](#concurrency).
- [ ] **Open question 4 — superseded head envelopes.** See [Superseded envelopes](#superseded-envelopes).
- [ ] **Open question 5 — identities on import.** See [Importing](#importing).
- [ ] **Phase 0 — admin surface.** The chosen surface, an `Admin` capability, and
  the server-side operation runner (one maintenance operation at a time per
  repositories directory).
- [ ] **Phase 1 — export.** A quiesced network written as a bundle: the pool's
  files, every head, and the content its heads reach. Bench case at 10,000 changes.
- [ ] **Phase 2 — import and adoption.** A bundle, or a `RepositoryDirectory` from
  `wheat import`, placed in a repositories directory with names; its content moved
  into the content store. Closes the fork-networks doc's adoption item.
- [ ] **Phase 3 — deleting a served network or fork.** Close the network in the
  registry, then `RepositoriesDirectory::delete`. Closes the fork-networks doc's
  "deleting a fork while its network is served" item, and completes a move (export,
  import elsewhere, delete here).
- [ ] **Phase 4 — pool GC.** Per network: mark every head's closure, rewrite
  segments keeping only marked records, retire unmarked loose objects and the pool's
  content records.
- [ ] **Phase 5 — content GC.** Across the directory: mark every head's
  `required_content` plus delta bases, rebuild the content store keeping only marked
  records.
- [ ] **Phase 6 — leftovers.** Networks and heads no name refers to (crash debris,
  see the fork-networks doc's Layout), past an age.
- [ ] **Defer — Gothab.** Gothab's content is in its blob store and its heads in its
  database; its GC marks the same way but sweeps through its own inventory. Revisit
  with Gothab's opener.
- [ ] **Defer — remote packed stores.** `remote_packed` has no retirement at all
  (append-only catalog, one S3 key per blob).

## Why this exists

Phase 1 of `wheat-fork-networks.md` put every network's content in one store,
`<repos-dir>/content`, so a network directory is no longer self-contained: copying
`networks/<id>` moves its objects and heads but not its content. Nothing ever
removes anything either: deleting the last repository of a network leaves its
content, each publish leaves the previous head's envelope and label manifest
unreachable, and a crash can leave a network no name refers to. The two problems
share their hard part, finding what a set of heads reaches, so they are designed
together. Both need the content store, which the serving process holds locked, so
both run inside the server.

## Goals

- Move a network, with every repository and its content, from one repositories
  directory to another, without stopping either server.
- Adopt what `wheat import` writes (a `RepositoryDirectory`) into a served
  repositories directory: the regression the fork-networks doc lists.
- Delete a network or fork while its server runs.
- Reclaim the bytes of objects and content no head reaches, with the fail-closed
  rules of `wheat-storage-efficiency.md`, "Dead object collection".
- Each operation's cost scales with what it moves or keeps, with bounded memory.

## Non-goals (v1)

- Gothab's GC (its blob store and database heads) and remote packed stores.
- Deciding history retention: GC never squashes or drops retained history; it only
  reclaims what no head reaches (`wheat-storage-efficiency.md`, Phase 4).
- Joining two networks, or splitting a fork out of its network.

## What exists to build on

- **Content a head reaches.** `RepositoryState::required_content` is cumulative:
  every blob any retained revision, application, conflict or view names, and a
  publish may only add to it (`increment_validation.rs`). `ContentSetStore::cursor`
  streams it with memory proportional to key length, not set size.
- **Objects a head reaches.** `snapshot.rs`'s `ClosureWalk` walks an envelope's whole
  closure with its visited set and stack in a temporary SQLite file, and
  `object_graph::for_each_reference` is the one list of what each object refers to.
  `StructuralDelta` (fetch, publish) keeps its visited sets in memory, capped at
  4,000,000 pending.
- **Rewriting the store.** `PackedCatalog::repack_structural` writes replacement
  segments, publishes a new catalog snapshot through `repack.intent`, and removes the
  old segments; content repack builds a sharded content generation from a list of
  IDs and retires the loose content it covers. Both keep every record today; neither
  takes a "keep only" set. Old segments are removed at once: there is no reader grace
  period anywhere.
- **Holding a network still.** `Registry::quiesce` refuses publishes to every
  repository of a network and waits for writes in progress. It does not hold the
  shared content store: other networks' publishes still write content.
- **Copying.** `NetworkDirectory::files` and `copy_to` (pool and heads),
  `RepositoryDirectory::copy_to`.

Missing: an admin access level or message (levels are `Read < Publish < Force`), any
grace period, and a function listing a blob's delta bases.

## Admin surface

**Open question 1.** How an operator (or a script) asks the server to export, import,
delete or collect. Options:

- **A. Admin messages on the protocol.** New request/response messages on the
  existing WebSocket and an `admin` capability in `[auth]` (a principal-wide flag,
  not a per-repository level, since import and GC are not about one repository). A
  `wheat admin ...` client command speaks them. Works remotely; adds protocol
  surface and an authorization level to review.
- **B. A local socket.** `wheat-server --admin-socket <path>`: a Unix socket whose
  file permissions are the authorization, speaking the same request records over a
  simple frame. Only on the host; no protocol or auth change; Windows needs a named
  pipe or none.
- **C. Library only.** `wheat_server::maintenance` functions an embedder calls
  (Gothab drives its own), with the standalone server exposing nothing yet.

Whatever the surface, the server runs one maintenance operation at a time per
repositories directory, and reports progress and the counts below.

## Moving a network

A move is export, import elsewhere, then delete here. The unit is a **bundle**: a
self-contained network, the shape a `RepositoryDirectory` already has with any number
of heads.

```text
<bundle>/
  storage/                          packed store: the pool's objects and the content
  heads/<repository>/repository.head
  bundle.manifest                   BinSchema: network ID, each repository's ID, format
```

`RepositoryDirectory` (what `wheat import` writes) is a bundle with one head at the
root and no manifest; import accepts both.

### Exporting

1. Quiesce the network, so its pool and heads stay still. Fetches go on.
2. Read every head.
3. Copy the pool's files into a hidden stage, as `NetworkDirectory::copy_to` does.
4. Open the staged store and write into its content side every blob the heads'
   states list in `required_content`, read through the content store as logical
   bytes (delta chains resolved), so the bundle carries no delta bases from outside
   it. Content is immutable, so this step need not hold publishes to other networks.
   The union of the heads' sets is streamed one head at a time, skipping what the
   staged store already holds.
5. Release the quiesce once the heads and pool are copied (after step 3; step 4
   reads only content).
6. Validate each head (`validate_complete`), write the manifest, sync, and rename
   the stage into place.

Cost: O(pool bytes) copied plus O(content reached) read and written; memory bounded
by the batch size. The network refuses publishes only for steps 1–3.

**Open question 2.** How a bundle gets to the other server:

- **A. A path on the server's disk.** Export writes to a directory the operator
  names; the operator copies it (rsync, scp) and imports from a path there. Simple;
  needs shell access to both hosts.
- **B. Streamed over the admin connection.** Export streams the bundle's files to
  the client, import accepts them from it, resumable by file. Needs option 1A;
  larger protocol surface.

### Importing

1. Validate the bundle: every head against its store (`validate_complete`), so a
   damaged bundle is refused before anything is placed.
2. Stream its content into the content store in batches, skipping what is there
   (`existing_ids`), each batch one durable append. This needs no hold on any
   network: content is only added.
3. Stage the network: copy the bundle's `storage/` and heads beside the first name,
   as `create_empty` stages, and rename it into `networks/<network>`.
4. Write each name (`names/<owner>/<repo>`), last, under the names' locks. A name
   that is taken refuses the import before step 3.

The copied pool still holds the bundle's content records, which nothing reads
(content is read from the content store); pool GC (phase 4) drops them.

**Open question 5.** Identities:

- **A. Keep the bundle's network and repository IDs.** A move keeps identity, so
  logs and any external references still match; an import of an ID already present
  is refused (a network imported twice).
- **B. Fresh IDs on import.** Two imports of one bundle are two networks; nothing
  can collide; identity does not survive a move.

### Deleting a served network or fork

`RepositoriesDirectory::delete` refuses while the network's store is open. The
maintenance runner instead: removes the name first (so new sessions find nothing),
asks the registry to drop the repository (or, for the last repository, the network)
once nothing holds it, waits for the store to close (`OpenNetworks` already signals
it), then tombstones the head or network as today. A fork's head can be removed
without closing the network: only the last repository's delete closes the store.

## Garbage collection

### What is reachable

- **A network's objects:** the closure of every head in the network, walked with a
  disk-backed visited set (`ClosureWalk`'s SQLite, generalised to many roots and to
  record marks rather than export them).
- **Content:** the union, over every head of every network, of each state's
  `required_content`, plus every delta base a marked record needs to be read,
  transitively to a fixed point (bounded by the 16-link chain limit). The union is
  disk-backed (a SQLite table or sorted runs) since the instance-wide set can exceed
  memory.

Marking fails closed: a head that does not validate, an object or set node that
cannot be read, or a limit reached stops the collection with nothing removed.

### Sweeping

- **Pool:** a structural repack whose inventory is the marked set: replacement
  segments hold only marked records, the pool's content-namespace records are all
  dropped (content lives in the content store), and unmarked loose objects are
  retired. This extends `repack_structural` with a keep-set; publication through
  `repack.intent` and crash recovery are unchanged.
- **Content store:** a content repack whose inventory is the marked set builds a new
  generation; segments holding content not in it are replaced, and loose content not
  marked is retired. The content store's segments hold content only, so their
  rewrite is the same keep-set repack.

Cost: marking is O(objects and set nodes reachable) reads per network and O(content
IDs) for the content union; sweeping is O(bytes kept) written. Bench cases: pool GC
of a network of 10,000 changes with a fork, and content GC across 20 networks,
counting records read and bytes written.

### Concurrency

**Open question 3.** Publishes write records before any head names them: a large
publish stages objects and content early, in batches, and a publish may name content
already in the store without uploading it (decision B of the fork-networks doc). A
record a mark did not find may be named by a head a moment later.

- **A. Hold the writers.** Quiesce the network for pool GC; for content GC, quiesce
  every network (publishes refused with a retryable error) from the start of marking
  until the new generation is published. Fetches go on. Simple and exact; publishes
  wait for the whole collection.
- **B. A write barrier.** While a collection runs, every commit records what its new
  state adds (`ContentSetStore::added` for content; the objects its publish walk
  received or found stored, for the pool) in the collection's mark set, and records
  written after the collection began are kept regardless. Publishes go on; the
  commit path gets a hook and the mark set takes concurrent inserts.

### Superseded envelopes

**Open question 4.** An envelope does not name the previous one, so every publish
leaves the old envelope and its label manifest unreachable. A client fetching with
`since` set to such an envelope after GC removed it gets a full fetch (the server
ignores a `since` it does not hold): correct, but O(repository) instead of
O(change). Options:

- **A. Collect them.** Accept full fetches for clients behind a collection.
- **B. Keep recent ones.** Mark, as extra roots, each repository's envelopes from
  the last N days. Needs the head store to remember envelopes it held (the publish
  receipts table keeps some of this for idempotent publishes).
- **C. Keep the last K per repository.** As B, by count.

### Reader grace

Fetches in progress read through open segment readers, which keep a removed file
readable until closed, and the catalog's exclusive lock orders a repack against
lookups in this process. Since GC removes only records no head reaches, a reader is
affected only when it reads an unreachable record (a superseded envelope, question
4). No further grace period is proposed for the in-process server; an external
reader of the files (backup by file copy) must not run during a collection.

## Performance

- Export: O(pool bytes) copied, O(content reached) streamed; publishes refused only
  while the pool is copied.
- Import: O(bundle bytes); content appended in batches, one durable append each.
- Pool GC: O(reachable objects) reads, O(kept bytes) written, disk-backed marks.
- Content GC: O(content IDs reachable) across the directory, plus a delta-base fixed
  point bounded by chain depth.
- Each phase adds a bench case at 10,000 changes counting records read, bytes
  written and the time publishes were refused.

## Phasing

Phases 0–3 make moving work (and close the fork-networks doc's adoption and deletion
items); 4–6 are GC. Phase 1 (export) needs no admin surface to be testable and could
start before question 1 is answered; everything user-facing waits for it.

## References

- `docs/design/wheat-fork-networks.md`: layout, Phase 1, and the outstanding items
  this closes.
- `docs/design/wheat-storage-efficiency.md`: Phase 4 (repack and GC contracts), "Dead
  object collection".
- `docs/design/wheat-native-protocol.md`: registry, quiesce, two-tier storage.
- `TODO.md`: `wheat gc`; server-authoritative remote maintenance; Forks.
