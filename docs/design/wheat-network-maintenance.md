# Wheat network maintenance: moving networks and collecting garbage — Design

Status: partial — Phase 0 (the admin surface), Phases 1 (export), 2 (import and
adoption) and 3 (delete while served) landed, so a network can be moved; Phase 2b
and GC (4–6) designed, not yet implemented; Gothab's content GC (2026-11-06) is
built on Wheat's side (marking, sweeping, and naming what publishes rely on), with
three gaps recorded below. Bart answered questions 1–5 on 2026-10-27; a
review by Codex and Antigravity the same day raised questions 6–8, which Bart
answered on 2026-10-28, and the changes in [Review](#review-2026-10-27).
Owner: bart
Last updated: 2026-11-07

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
- [x] **Decision — admin surface (2026-10-27).** Open question 1, option A: admin
  messages on the protocol, an admin flag per principal, `wheat admin`.
- [x] **Decision — transport (2026-10-27).** Open question 2: one bundle format,
  structure apart from content, carried both ways: a directory on disk and an
  S3-compatible bucket, as Gothab moves Git repositories.
- [x] **Decision — GC holds writers (2026-10-27).** Open question 3, option A.
- [x] **Decision — superseded envelopes are collected (2026-10-27).** Open question
  4, option A.
- [x] **Decision — import keeps IDs (2026-10-27).** Open question 5, option A.

- [x] **Decision — a move refuses writes (2026-10-28).** Open question 6: from
  export until the source is deleted the network refuses publishes with a retryable
  error carrying a retry delay from the move's expected duration.
- [x] **Decision — content GC holds only for irreversible steps (2026-10-28).** Open
  question 7, option B, with a bounded rescue list.
- [x] **Decision — fetches read the old store until done (2026-10-28).** Open
  question 8, option A.
- [x] **Phase 1 — export to a directory (2026-10-28).** `RepositoriesDirectory::export_network`
  copies a quiesced network's structure into a stage; `StructureExport::finish` lists
  and copies the content its heads reach, validates every distinct state, and installs
  the bundle with its manifest written last (`wheat-store/src/network_bundle.rs`,
  `tests/network_bundle.rs`). Bench case `network_export`: at 10,000 changes the copy
  takes 0.3–2.5 s and the finish 20–27 s, of which about 20 s is `validate_complete`
  re-verifying the history (2 ms a change); peak RSS 460 MiB. Not yet wired to a
  server operation: that is Phase 0.
- [x] **Phase 2 — import and adoption from a directory (2026-10-28).**
  `RepositoriesDirectory::import_network` places a bundle's network under its own IDs
  and `adopt_repository` places a `RepositoryDirectory` under new ones
  (`wheat-store/src/network_import.rs`, `tests/network_import.rs`). Differs from the
  review's "durable import record": the network's rename into `networks/` is the commit
  point (a stage validated against the content store, never renamed over another
  network), and the names follow it under locks taken first. A crash between leaves
  a network no name refers to; the same import again finds it (no name outside the
  request points into it) and writes the names, so no record is needed. Every
  manifest path is checked (only `storage/...` and the listed heads) and every file
  is hashed against the manifest as it is copied. Bench case `network_import`: 10,000
  changes in about 21 s, nearly all `validate_complete`; peak RSS 450 MiB. Not yet
  held against content GC's publish gate (Phase 5).
- [x] **Phase 3 — deleting a served network or fork (2026-10-29).** `Registry::delete`
  (and `wheat admin delete`, `AdminDeleteCommand`, kind 4): marks the name deleting so
  `get` finds nothing, drops the registry's slot, signals the entry removed so sessions
  on it end with a connection-scoped 404, then retries `RepositoriesDirectory::delete`
  until the store has closed (up to 30 s; `DeleteError::Held` past that). As review
  item 5 asked, removing a fork's head no longer needs the network's store closed;
  only the last repository, which takes the network, does. Concurrent deletes of a
  network's last two repositories serialize on a `members.lock` in the network
  directory, so one of them sees it is last. Differs from the design: no draining
  state on the network, since a fork's delete never closes the store and the last
  repository's delete waits for the close by retrying the store lock rather than
  through `OpenNetworks`. A delete needs no exchange directory. Tests:
  `wheat-server/tests/delete_test.rs`, `wheat-store/tests/repositories_directory.rs`,
  and `remote_admin.rs`'s `deleting_a_served_repository_ends_a_move` (export, import
  elsewhere, delete here, clone from the new server).
- [x] **Delete through any opener (2026-11-05, beyond the original plan).** Gothab
  serves through its own `RepositoryOpener`, with no repositories directory.
  `Registry::delete` now waits for the entry's holders itself and then calls the
  opener's `RepositoryDeleter`; `DirectoryOpener`'s deleter is the retry loop over
  `RepositoriesDirectory::delete` described above. Tests:
  `an_opener_deletes_its_own_repository_once_the_store_closed` and
  `an_opener_without_a_deleter_leaves_its_sessions_alone` in `delete_test.rs`.
- [x] **Retry delay on refused publishes (2026-10-29).** The Error record has
  `retry_after_seconds` (0 = no estimate). A holder calls `Quiesced::expect(duration)`;
  publishes refused while it is held get the time left, and the CLI prints it. Export
  estimates its structure copy as the files' bytes over 50 MiB/s (a conservative guess,
  not a measurement; floor 1 s). Moves holding longer, and GC, set their own estimates
  when they exist.
- [x] **Phase 0 — admin surface (2026-10-28).** Four messages on the protocol
  (`AdminStart`, `AdminStarted`, `AdminStatusRequest`, `AdminStatus`; types 0x50–0x53;
  `wheat-protocol.bs.json5`), an `admin = true` flag on a principal in `[auth]`
  (`AccessProvider::is_admin`), the operation runner (`wheat-server/src/admin.rs`)
  and `wheat admin --server <url> export | import | adopt` (`wheat-client/src/remote_admin.rs`).
  Built early so the operations can be driven through the real interface while their
  shape may still change; GC has no message yet. Tests: `remote_admin.rs`
  in `wheat-cli` (export on one server, import on another, adoption, confinement,
  admin-only), unit tests for the operation table, a protocol round trip. Differences
  from the design:
  - Admin sessions use a reserved WebSocket path, `/.admin` (`ADMIN_PATH`); a
    repository cannot be named `.admin`. `wheat admin --server wheat://host:port` connects
    there. The session authenticates like any other and then asks `is_admin` before
    every request, so a right revoked stops the next one.
  - Bundles and adoption sources are names in the server's `--exchange-dir` (one plain
    segment, no leading dot), so an admin cannot make the server read or write elsewhere.
    Without `--exchange-dir` the server refuses the three operations (501).
  - One operation runs at a time per server (409, retryable, while another runs); the
    last 64 finished operations stay pollable. An operation outlives the connection;
    its ID is not found again after a restart.
  - Import names repositories by their ID in the bundle: `--name <id>=owner/repo`,
    one per repository; the export's message lists `repository <id> <name>` lines.
  - Export waits up to 60 s for in-flight publishes before giving up.

### Outstanding

- [ ] **Phase 2b — the bucket transport.** Export to and import from an
  S3-compatible bucket: immutable store files once by name, content once by ID,
  shared by every export to the bucket.
- [ ] **Phase 4 — pool GC.** Per network: mark every head's closure, rewrite
  segments keeping only marked records, retire unmarked loose objects and the pool's
  content records.
- [ ] **Phase 5 — content GC.** Across the directory: mark every head's
  `required_content` plus delta bases, rebuild the content store keeping only marked
  records.
- [ ] **Phase 6 — leftovers.** Networks and heads no name refers to (crash debris,
  see the fork-networks doc's Layout), past an age.
- [ ] **Gothab content GC (2026-11-06, was deferred).** Bart put it first, ahead of
  pool GC (Phase 4) and the directory's content GC (Phase 5). Mark every Wheat head
  of the instance, sweep Gothab's blob store through a delete hook; see
  [Gothab's content](#gothabs-content-2026-11-06). G1–G4 decided 2026-11-06; the
  steps are listed there.
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

Missing: any grace period, and a function listing a blob's delta bases.

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

Bart chose A (2026-10-27). Operations run in the server and outlive the request that
started them: a request starts one and returns its ID; another asks for its progress
and result, so a dropped connection loses nothing.

## Moving a network

A move is export, import elsewhere, then delete here. The unit is a **bundle**: a
network's **structure** (its pool's files and every head), the list of **content**
its heads reach, and that content, kept apart so a host whose content store is
already shared (Gothab's blob store) moves only the structure. This is how Gothab
moves Git repositories (`gothab-storage`'s structural archive, `blob_refs.txt`, and
blobs once each under `blobs/<hash>`; nothing goes from host to host).

```text
<bundle>/                                     a directory, or a prefix in a bucket
  bundle.manifest                             BinSchema: format, network ID, each
                                              repository's ID, content list digest
  structure/storage/                          the pool's files
  structure/heads/<repository>/repository.head
  content.list                                BinSchema: the ContentIds reached, sorted
  content/                                    the listed content
```

In a directory, `content/` is a packed store. In a bucket, content is one object per
blob, `<prefix>/content/<2 hex>/<rest>` (the key `remote_packed` already uses),
shared by every export to the bucket so a blob is uploaded once; immutable store
files (`RepositoryFile::immutable`: sealed segments, catalog snapshots, content
generations) are likewise keyed by name under `<prefix>/networks/<network>/files/`
and uploaded once, while the manifest, `catalog.head`, the active segment and the
heads go under `<prefix>/networks/<network>/exports/<export>/`, the manifest last.

`RepositoryDirectory` (what `wheat import` writes) is a bundle with one head at the
root, content inside its store and no manifest; import accepts it as well.

### Exporting

1. Quiesce the network, so its pool and heads stay still. Fetches go on.
2. Read every head and copy the structure: the pool's files, as
   `NetworkDirectory::copy_to` lists them, and the heads.
3. Release the quiesce. What follows reads only content, which is immutable, and
   the copied structure.
4. Write `content.list`: the union of every head's `required_content`, streamed with
   `ContentSetStore::cursor` and merged through a disk-backed sort, so memory is
   bounded however many blobs the network reaches.
5. Copy the listed content as logical bytes (delta chains resolved), so the bundle
   holds no delta base from outside it; skip what the destination already holds
   (the bucket's keys, checked in batches).
6. Validate each head against the structure and content (`validate_complete`),
   write the manifest last, and sync (a directory is staged and renamed into
   place).

Cost: O(pool bytes) copied plus O(content reached) read and written; memory bounded
by the batch size. The network refuses publishes only for steps 1–2.

**Open question 2.** How a bundle gets to the other server:

- **A. A path on the server's disk.** Export writes to a directory the operator
  names; the operator copies it (rsync, scp) and imports from a path there. Simple;
  needs shell access to both hosts.
- **B. Streamed over the admin connection.** Export streams the bundle's files to
  the client, import accepts them from it, resumable by file. Needs option 1A;
  larger protocol surface.

Bart asked how Gothab moves Git repositories (above), and chose (2026-10-27) one
bundle format with structure apart from content, and two transports: a directory
(phase 1, 2) and an S3-compatible bucket (phase 2b). Streaming over the admin
connection is not built.

### Importing

1. Read the manifest and validate the bundle: every head against its structure and
   content (`validate_complete`), so a damaged bundle is refused before anything is
   placed.
2. Refuse if the network ID, a repository ID or a name is already present.
3. Add the listed content to the content store in batches, skipping what is there
   (`existing_ids`), each batch one durable append. This needs no hold on any
   network: content is only added.
4. Stage the network: copy the structure beside the first name, as `create_empty`
   stages, and rename it into `networks/<network>`.
5. Write each name (`names/<owner>/<repo>`), last, under the names' locks.

A `RepositoryDirectory`'s pool still holds its content records once copied, which
nothing reads (content is read from the content store); pool GC (phase 4) drops them.

**Open question 5.** Identities:

- **A. Keep the bundle's network and repository IDs.** A move keeps identity, so
  logs and any external references still match; an import of an ID already present
  is refused (a network imported twice).
- **B. Fresh IDs on import.** Two imports of one bundle are two networks; nothing
  can collide; identity does not survive a move.

Bart chose A (2026-10-27). A `RepositoryDirectory` has no IDs; its import makes new
ones.

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

Bart chose A (2026-10-27). Content GC holds every network of the directory, and so
refuses every publish, from marking until the new content generation is published;
the bench case records how long.

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

Bart chose A (2026-10-27): only the current heads are roots.

### Reader grace

Fetches in progress read through open segment readers, which keep a removed file
readable until closed, and the catalog's exclusive lock orders a repack against
lookups in this process. Since GC removes only records no head reaches, a reader is
affected only when it reads an unreachable record (a superseded envelope, question
4). No further grace period is proposed for the in-process server; an external
reader of the files (backup by file copy) must not run during a collection.

## Gothab's content (2026-11-06)

Gothab keeps every Wheat repository's content in one instance-wide blob store
(`ContentBlobStore`: put, get, get_range, list_page) and a `wheat_content_sizes`
row per content; heads are in its Postgres, and each fork network's structure pool
is on the disk of the one gitd host that serves it. Deleting a repository, or a
network's last one, leaves its content, and nothing reclaims it. Bart chose
(2026-11-06) to collect this first, before pool GC.

### Shape

1. **Mark, per host (Wheat).** For the heads a host serves, the union of each
   state's `required_content`, skipping set nodes already visited by ID (states
   share most of their sets), into a disk-backed mark set. Then, to a fixed point,
   what reading a marked record needs: a delta record's base, and a chunk manifest's
   chunks and parts (`wheat-large-content.md`). Fails closed, as above: an unreadable
   head, set node or record stops the collection with nothing deleted.
2. **Merge (Gothab).** Each host's mark set as a sorted ID stream; Gothab unions
   them, so the sweep sees every head of the instance.
3. **Sweep (Wheat logic, Gothab's inventory).** Walk `list_page` and, for each ID
   not marked and not rescued (below), call the embedder's
   `reclaim(id)`, which deletes the blob and its `wheat_content_sizes` row.

### Publishes during a collection

As in open question 7, a publish may name content the mark did not find, without
uploading it (`existing_ids`), so it must be rescued. Two cases:

- **Content a publish names.** Publish validation already computes what the new
  state's `required_content` adds (`increment_validation.rs`, step 7,
  `added_content`), bounded by the publish.
- **Records a publish writes.** A put replaces a stored record (`content_blobs.rs`),
  and a delta's base need not be required by any state. A publish that stores X as
  a delta of an unmarked Y while a collection runs makes X unreadable if Y is swept.
  So what a publish uploads must rescue its delta bases and chunks too, and content
  uploaded for a publish still in progress (named by no head yet) is kept by age.

### Decisions (2026-11-06)

Bart deferred to @wise-walrus with a lean to G1 (a); walrus relayed these answers.

- **G1 — a last-named time.** Gothab keeps `last_named_at` on `wheat_content_sizes`.
  Its `ContentBlobStore::put` stamps everything a publish writes (deltas, chunks,
  manifests). A Wheat hook names what a publish references without writing: content
  found already stored, and each upload's delta base. The alternative, an
  instance-wide hold, was not taken.
- **G2 — the stamp at put is the upload's age.** `BlobPage` stays without times.
- **G3 — recorded at put.** gitd stores each record's references (delta base, a
  manifest's chunks or parts) on its row, decoded by a Wheat function so the format
  has one decoder; marking reads them from Postgres.
- **G4 — split.** Each gitd marks the networks it serves (an admin task) and writes
  a mergeable mark run; Gothab's maintenance worker, under a lock, merges the runs
  and sweeps, holding no pools.

### API

- `wheat_store::record_info(id, record) -> RecordInfo { logical_length, references }`,
  `RecordReferences::{None, Delta { base }, Chunks(ids), Parts(ids)}`, for put time.
  A part is an ordinary blob, so its chunks are found while sweeping, by reading it.
- `ContentMarks` (SQLite at a path): `create`, `mark_heads(reader, envelopes)` (the
  union of each state's `required_content`, set nodes visited once), `finish` and
  `merge(path, files)`. Changed from the shape first sent (a run file of sorted IDs):
  the finished SQLite file is what a host ships, and the worker merges files with
  `ATTACH`, so there is no run format to define. A file is used only once finished;
  marks are written unsynced and synced before the finished row is.
- `sweep(blobs, marks, inventory)`: closes the marks over `inventory.references`
  (and parts read from `blobs`) to a fixed point, then pages `list_page` and calls
  `inventory.reclaim(id)` for each ID not marked.
- Changed at walrus's request (2026-11-07). `reclaim(ids)` takes a listed page's
  unmarked IDs at once (at most `SWEEP_PAGE_IDS`, 1,000) and answers those it deleted;
  `SweepCounts` counts pages, reclaim calls, and what the inventory kept. One call
  per ID meant a Postgres round trip each, millions after a large delete. A mark
  file records the collection's start (`ContentMarks::create(path, started)`,
  `started()`), and `merge(path, started, inputs)` refuses an input marked for a
  collection that began earlier.
- A `ContentNaming` hook on wheat-server's network, `named(ids)`, called before a
  publish or server write joins the commit with what its state adds and its uploads'
  delta bases. A failure, including an ID Gothab has no row for (reclaimed since the
  publish found it), refuses the publish as retryable.

#### Naming what reading needs (2026-11-07)

Naming a record is not enough when what reading it needs is old: a chunked blob's
chunks and parts, a part's chunks, or a delta's base. Suppose a head is committed
after a mark read the heads. The manifest it relies on survives on its fresh stamp,
while chunks it shares with dead content are swept. Bart chose (2026-11-07) that Wheat
names the closure, so Gothab's `named` only stamps rows. As built:

- `ContentStore::needed_to_read(ids, bases)` (`wheat-store/src/content_closure.rs`)
  gives `ids` and everything reading them needs. It reads each record's header and a
  delta's base reference in one ranged read (`record_references`), a chunked blob's
  manifest whole, and each part once. Chunks and parts are stored whole, so they are
  not read. Of `bases`, a chunked one is left out, since a chunked blob is no delta's
  base.
- `wheat_server::naming::ContentNaming { renew_after(), named(ids) }`, set with
  `Network::with_naming` or `OpenNetworks::with_naming`. A publish takes the instant
  before it plans its content. Once it is checked, it names the content its state adds
  that was stored already, the manifests it uploaded, and its uploads' delta bases,
  all through `needed_to_read`. It does this before it joins the commit.
  A failure answers the publish with the error status ("publish again"), and nothing
  is kept. A server write adds no content and names nothing.
- A batch keeps what its publishes named and uploaded, and the oldest of their
  instants. Just before its head compare-and-swap, if that instant is older than
  `renew_after` (half the grace), it names all of it again; a failure fails the
  batch. Uploads are stored by then, and their put stamps are newer than the instant.
- Cost per publish: one ranged read per added ID found stored and per delta base,
  one read per uploaded or existing manifest and per part, and one `named` call.
  A renewal adds a second call, without reads. `tests/content_blobs.rs` counts the
  reads, and `tests/naming_test.rs` covers the hook end to end.

### Timing and races

- A stamp taken before the head moves can be older than a mark that begins before
  the commit and so misses the head. The sweep therefore reclaims only content last
  named before the mark's start minus a grace G, and Wheat names a batch member's
  content again when its stamp is older than G/2 as the batch commits. Every head
  committed after the mark started then has its content stamped after start − G.
  G is configurable, an hour by default; each host reads its heads after the start.
- Gothab serializes put and reclaim per ID on the row: reclaim locks the row, checks
  it old, deletes the blob, then the row; put upserts the row first, then writes.

### Outstanding (Gothab content GC)

- [x] `record_info` and `RecordReferences` (fdf8ded).
- [x] `ContentMarks`: mark, finish, merge, with `sweep` and the `ContentInventory`
  trait (`wheat-store/src/content_gc.rs`, `tests/content_gc.rs`). A child is checked
  against the visited set before it is read, so a shared subtree costs no read.
- [x] Bench case `content_gc` (2026-11-06, 10,000 changes): marking 20 heads spread
  along the history reads 5,106 objects, 51% of marking each alone (budget 60%);
  20 forks one change past the newest add 85 reads to its 951 (budget 10 a fork); a
  sweep of 110,220 listed IDs reclaims the 100,200 dead in 0.29 s with 10 inventory
  calls. With batched reclaims (2026-11-07) that is 111 reclaim calls, within the
  budget of one a listed page.
- [x] The `ContentNaming` hook in wheat-server's publish path, with the G/2 renaming,
  naming what reading the named content needs (2026-11-07, above).
- [ ] **Gap — a chunk stored as a delta.** A chunk whose bytes equal content stored
  earlier as a delta is reused as that delta, and naming does not read chunks, so
  its base is not named (TODO.md).
- [ ] **Gap — a join after a forced publish.** The union can re-add content a forced
  publish dropped before a mark began, which nothing names (TODO.md).
- [ ] **Delta bases at publish come from the base view.** So publishes hardly ever
  store deltas, and naming has few bases to name; a perf gap of publishing, not of GC
  (TODO.md).

## Review (2026-10-27)

Codex and Antigravity reviewed this design at Bart's request. What they found, checked
against the code, and how the design changes. The sections above stand as written;
where they disagree with this section, this section wins.

### Changes

1. **A directory-wide publish gate.** `Registry::quiesce` holds one network's
   `Writers`, and a network opened later gets fresh ones (`network.rs`, `Network::new`),
   so "quiesce every network" misses networks opened during a content GC: a publish
   to one finds a blob present (`publish.rs`, the `existing_ids` check), skips
   uploading it, and the sweep removes it. Content GC instead takes a gate on the
   registry that every publish passes (`Writers::begin` consults it), including those
   to networks opened after the gate closed, and that `RepositoriesDirectory::create_empty`,
   `fork` and import respect. Pool GC and export keep the per-network quiesce, which
   the network's own opens share.
2. **Unreachable before writers resume.** Publishing the new content generation is
   not enough: loose content left behind still answers `contains` and reads
   (`store.rs`, the loose fallback in `logical_length` and `read_stored`). The hold
   ends only once every swept record is out of every lookup path (the catalog,
   loose index, and loose fallback); files are removed after (question 8).
3. **Bucket keys and an authenticated file list.** Content-generation shard files are
   named `<generation>/shard-N.pack` with no digest (`content_generation.rs`,
   `content_generation_shard_manifest_path`), so one name can hold different bytes
   after a restore and a repack. Immutable files are keyed by their relative path
   and BLAKE3 digest, and the manifest lists every structural file with path, length
   and digest; import verifies each as it downloads. An object already in the bucket
   is reused only when its digest matches.
4. **Import as a resumable transaction.** Every name's lock is taken before anything
   is checked and held to the end; the IDs and names are checked before the
   expensive validation; a durable import record beside the first name lets a crash
   resume (or roll back) the same import, rather than leaving a network a retry
   refuses because its ID is taken.
5. **Deleting while served.** `RepositoriesDirectory::delete` takes the closed-store
   lock before checking for other repositories and finds the head through the name,
   and `Registry::get` serves a cached entry without reading the name. Deletion
   instead marks the registry's entry (or network) draining, so `get` refuses it and
   its publishes end, waits for holders to drop it, then removes head and name under
   the name's lock with the location already read; removing a fork's head no longer
   needs the network's store closed. `OpenNetworks` exposes waiting for a network's
   close.
6. **Dropping the pool's content.** `repack_structural` drops content records only
   when a validated replacement generation covers them (`catalog.rs`,
   `may_retire_content`), and `retire_repacked_content` does nothing without one. Pool
   GC adds an explicit mode that drops every content record, generation and loose
   content file of a network's pool, allowed once the content store is checked to
   hold each of them.
7. **Bounded memory and lookups during a rewrite.** Content repack caps planning at
   250,000 IDs and takes its inventory as a `Vec` (`ContentRepackLimits`,
   `ContentRepackInput`); structural repack collects its inventory in memory;
   `validate_complete` builds lineage in memory and reads each blob once per head; and
   repack holds the catalog's exclusive lock through validation and rewrite, so
   fetch lookups wait. GC streams its inventories from the disk-backed mark set,
   validates content once per network rather than per head, builds replacements
   outside the lookup lock and takes it only to publish. Bench cases cover a million
   content IDs, a mostly dead store and a large-blob store, with memory, I/O and
   publish-hold budgets.
8. **A delta-base reader.** Marking reads a blob's delta base from its envelope
   header or generation entry without resolving the chain.

Not taken: Antigravity's note that the content store has no segments. It does: the
content store is a packed store, and content records of kind 0 go to its segments
(`store.rs`, reads through `CONTENT_NAMESPACE`).

### Open question 6 — closing a move

Bart chose (2026-10-28) a stricter form of A: a move refuses publishes from export
until the source is deleted, and tells the client when to retry, from the move's
expected duration. The refusal needs a retry delay in the error (see Outstanding).

Export releases the quiesce, so a publish after it reaches only the source, and the
delete that ends a move loses it. Options:

- **A. Read-only from export.** A move keeps the network quiesced from export to the
  delete; fetches go on, publishes are refused for the whole move. A plain export
  (backup) still releases early.
- **B. Check at cutover.** Export releases as designed; cutover quiesces again and
  refuses the delete unless every head is unchanged since the export.
- **C. Catch up at cutover.** Cutover quiesces again and ships only what changed since
  the export (new heads, new immutable files, new content), then deletes. Publishes
  are held only for the catch-up.

### Open question 7 — how long content GC holds publishes

Bart chose B (2026-10-28): no lock for the whole duration, only around irreversible
steps on the whole store. Records that become used while the collection runs lose
their eligibility: each commit during a collection adds the content its new state
adds (`ContentSetStore::added`, which skips shared subtrees, so it is bounded by the
publish) to a rescue list, and the final hold marks those too. The list is bounded
by the publishes admitted meanwhile; if it outgrows a limit, the collection stops
without sweeping and starts over. Records written but not yet named by a head (staged
early, or uploaded for a publish in progress) are kept by age: nothing newer than
the first hold is swept.

Holding every network from marking to publishing the new generation refuses every
publish of the directory for the whole collection, which Antigravity estimates at
tens of minutes on a large instance. Options:

- **A. Hold throughout**, as decided for question 3; the bench case records the hold.
- **B. Two short holds.** Hold briefly to read every head, then mark and build the
  replacement while publishes go on (records written meanwhile are kept); hold again
  to mark what commits since then added, make the swept records unreachable, publish
  and release. Part of the write barrier question 3 did not choose.

### Open question 8 — fetches in progress during a sweep

Bart chose A (2026-10-28).

A fetch looks objects up as it goes (`fetch.rs`, `answer_want`), and a repack replaces
the catalog's readers and removes old segments at once (`catalog.rs`, after the
index is rebuilt), so a fetch begun before a sweep can lose what it is sending, such
as a superseded `want` envelope. Options:

- **A. Read the old store until done.** A fetch begun before the sweep keeps reading
  through the store's previous readers and index; fetches begun after it, and every
  publish's lookup (change 2), see only the new one. The old files are removed once
  the last earlier fetch ends.
- **B. Pin fetch roots.** Each fetch in progress adds its `want` envelope to the
  roots until it ends.
- **C. Fail and retry.** A fetch that meets a swept record ends with a retryable
  error, and the client fetches again from the current head.

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
items); 4–6 are GC. Export and import (phases 1, 2, 2b) are library operations in
`wheat-store` with their own tests and bench cases, so they can land before the admin
surface (phase 0) that starts them in a server.

## References

- `docs/design/wheat-fork-networks.md`: layout, Phase 1, and the outstanding items
  this closes.
- `docs/design/wheat-storage-efficiency.md`: Phase 4 (repack and GC contracts), "Dead
  object collection".
- `docs/design/wheat-native-protocol.md`: registry, quiesce, two-tier storage.
- `TODO.md`: `wheat gc`; server-authoritative remote maintenance; Forks.
