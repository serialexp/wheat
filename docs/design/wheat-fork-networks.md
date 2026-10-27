# Wheat fork networks — Design

Status: draft, not yet implemented. Decided: one structural pool per fork network
and one instance-wide content store (Bart, 2026-10-27). Open: the questions below.
Owner: bart
Last updated: 2026-10-27

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they land
and move them from Outstanding to Done.

### Done

- [x] **Decision — structure (2026-10-27).** One packed structural pool per fork
  network; a fork is a new head in its network's pool (option F2 of three: copy on
  fork, pool per network, layered stores).
- [x] **Decision — content (2026-10-27).** One content store for the whole instance
  (option C3 of three: per repository, per network, instance-wide).

### Outstanding

- [ ] **Open question 1 — what a fetch may read from the pool.** See
  [Fetch](#fetch).
- [ ] **Open question 2 — what a publish must upload.** See [Publish](#publish).
- [ ] **Open question 3 — which labels a fork starts with.** See
  [Creating a fork](#creating-a-fork).
- [ ] **Phase 0 — networks in the registry.** An opener hands out entries whose
  stores are one shared handle per network; `Registry::quiesce` holds a network.
- [ ] **Phase 1 — the content tier apart.** Entries read and write content through
  an instance-wide `ContentStore` (Gothab's blob store through
  `ContentStore::over_blobs`, a directory for the standalone server); the network
  pool holds structure only.
- [ ] **Phase 2 — reads limited to the repository.** Content fetch checks
  `state.required_content`; object fetch per open question 1.
- [ ] **Phase 3 — publishes prove what they add.** Per open question 2, for
  objects and content.
- [ ] **Phase 4 — creating a fork.** A library operation and the registry call a
  host makes; tests for a fork of a fork and for a private fork's isolation.
- [ ] **Phase 5 — copying a network.** Quiesce and copy a network, with every head
  in it, as the unit of backup and move.
- [ ] **Defer — network GC.** Marking every head in a network and every network for
  content is the open `wheat gc` / server-authoritative maintenance work in
  `TODO.md`.
- [ ] **Defer — detaching a fork.** Copying one repository's closure into a network
  of its own (for a visibility change, say).

## Why this exists

Gothab forks are repositories that share history with their parent. Wheat today gives
every served repository its own packed store, objects and content behind one catalog
lock (`RepositoryDirectory`, `crates/wheat-store/src/repository_directory.rs`). A
fork would be a full copy (`RepositoryDirectory::copy_to`): O(repository) bytes and
time per fork, and no sharing afterwards.

The protocol doc already sketches the target, "Two-tier storage: global content,
per-network structural objects" (`wheat-native-protocol.md`): content stored once for
the whole instance and served only to clients whose repository references it, and one
structural pool per fork network, where "a fork is a new head pointer into the same
structural object pool. Creating a fork copies zero bytes." This doc turns that into
work, and records what sharing breaks in the server as it stands.

## Goals

- Creating a fork costs O(1): a new head naming the parent's envelope.
- A fork's objects and blobs are stored once however many forks share them.
- A client of one repository reads nothing it could not read before forks existed:
  nothing reachable only from another repository of the network, and no blob only
  another repository references.
- A publish to one repository never makes another repository's private objects or
  blobs part of its history without the client having them.

## Non-goals (v1)

- Moving a repository between networks, or joining two networks.
- Reclaiming space (network GC is deferred, as `wheat gc` is).

## What sharing breaks today

Object and content IDs are BLAKE3 hashes (`crates/wheat-core/src/id.rs`), so the same
history has the same IDs in every repository, which is what makes a shared pool
deduplicate. But the server trusts the store's contents to be the repository's:

- **Content fetch** (`handle_content_fetch`, `crates/wheat-server/src/fetch.rs`)
  serves any `ContentId` in `entry.content`. Over a shared content store that is any
  blob of the instance. (Also in `TODO.md`, "Content fetch checks no reachability".)
- **Object fetch** accepts any envelope in the store as `want` and as `since`
  (`handle_fetch`: "Any envelope in this repository's store may be fetched"), and
  answers an `ObjectWant` for any object the store has (`answer_want`). Over a
  network pool, a client of fork A can fetch private fork B's head envelope and
  everything below it.
- **Publish** asks the client only for what the store lacks: objects the structural
  walk from the current head finds missing (`receive_objects`), and blobs the new
  state adds that `existing_ids` does not find (`crates/wheat-server/src/publish.rs`).
  Over a shared store a client can name objects or blobs of another repository
  without sending them; once its head refers to them, fetching them from its own
  repository is legitimate.

## Domain model

- A **network** is a parent repository and every fork made from it or from its
  forks. It has one structural pool: a packed object store, as today's
  `storage/packed`, holding objects only.
- A **repository** in a network is a name and a head (`HeadStore`,
  `crates/wheat-server/src/heads.rs`). Its closure is everything reachable from its
  head's envelope. What it may read is its closure; nothing else in the pool.
- **Content** lives in one instance-wide store. A repository may read a blob when
  its head's state lists it in `required_content` (every blob a retained tree, edit
  or conflict names), an O(log n) set lookup.
- A network lives on one gitd host, so its pool has one writer process; forks are
  placed on their parent's host. Every repository of a network shares that process's
  one open handle to the pool, and the packed catalog already serializes writers
  within a process (`SharedCatalog`).

## Layout

Gothab keeps heads in its database (`HeadStore`) and content in its blob store; the
host's disk keeps each network's pool. For the standalone server's `DirectoryOpener`:

```text
<repos-dir>/
  content/                  instance-wide content store
  networks/<network>/
    storage/                the network's structural pool
    heads/<owner>/<name>/repository.head
```

How a repository name maps to its network is the opener's business: Gothab's
database knows each repository's network.

## Creating a fork

A fork is created on its parent's host:

1. Read the parent's head.
2. Create the fork's head at generation 0 naming the parent's envelope. Nothing is
   written to the pool.

The fork then holds every label the parent had. **Open question 3:** keep all of
them, as GitHub forks do by default, or start the fork with only the parent's default
label, which costs a new label manifest, envelope and state (a few objects written to
the pool, O(1) with respect to history)?

## Fetch

Content fetch checks the requesting repository's `state.required_content` for each
blob before serving it.

Object fetch: **open question 1**. A fetch names a `want` envelope and a `since`
envelope, the server pushes the closure of `want` less what `since` covers, and the
client then asks (`ObjectWant`) for objects it found missing. Options:

- **A. Head only, pulls re-checked.** `want` must be the repository's current head
  envelope. `since` is used only when it is an envelope this repository's head has
  held (the head store keeps them, as it keeps publish receipts, or the client's
  previous `want` is remembered per session), otherwise ignored. Each pulled object
  must be one the push walk visited in this fetch (the walk records the IDs it
  visits; memory O(objects visited)).
- **B. A reachability index per repository.** Record, per publish, the objects the
  publish added to the repository's closure, in an index on the host (SQLite, like
  the frontier index). A fork starts with its parent's set by reference. Any want
  is answered when the object is in the requesting repository's set. Costs a write
  per added object per publish and an index to keep with the pool.
- **C. A hash is a capability.** Serve any object of the pool, as today. A client
  can only name an object it has seen the bytes of, or been told about. Rejected by
  the protocol doc for private forks ("a client authenticated to fork A cannot read
  structural objects only reachable from private fork B"); listed for completeness.

## Publish

**Open question 2:** what must a client send?

- **A. Everything new to the repository.** Ask for every object the structural walk
  from the repository's current head finds outside that head's closure, and every
  blob the new state adds to `required_content`, whether or not the pool or the
  content store already has it. The server checks each against its ID and skips
  writing what is already stored, so storage still deduplicates; the upload does
  not. A fork pushing commits its parent already has uploads them again.
- **B. Only what the store lacks, checked against what the repository could read.**
  Ask for what the store lacks, as today, and accept a reference to a stored object
  or blob only when the repository could already read it (option B or A of open
  question 1 for objects; for blobs, membership in the network's other repositories
  is not something this repository may rely on, so in practice blobs fall back to
  upload).

Either way, a server-side merge (a Gothab pull-request merge between forks of one
network) runs on the host and needs no upload.

## Copying and quiescing

A repository's bytes are its network's: `Registry::quiesce` holds every repository of
the network, and a copy is the pool plus every head. Phase 5 extends today's
quiesce-and-copy (`crates/wheat-server/src/quiesce.rs`,
`RepositoryDirectory::copy_to`).

## Performance

- Creating a fork: O(1), one head write (open question 3's second option adds a few
  objects).
- Content fetch: one `required_content` lookup per blob, O(log n) set-node reads.
- Object fetch, option A: the walk is today's; the visited set adds memory
  proportional to the objects pushed. Option B: one index lookup per pulled object
  and one index write per object a publish adds.
- Publish, option A: upload bytes for objects and blobs new to the repository even
  when stored; the walk is today's.
- Each phase adds a benchmark case at its highest boundary: forking a repository of
  20,000 changes (constant), a content fetch of a page of blobs (lookups counted), and
  a publish to a fork (objects and bytes asked for, counted).

## Phasing

See the Outstanding list. Phase 0 and 1 can land before the open questions are
answered; phases 2 and 3 depend on them; phase 4 needs 2 and 3 so that no fork is
created while sharing still leaks.

## References

- `docs/design/wheat-native-protocol.md`, "Two-tier storage" and its resolved
  questions.
- `docs/design/wheat-storage-efficiency.md`, cross-repository sharing and GC scope.
- `TODO.md`, "Hosting on Gothab": Forks; content fetch reachability; `wheat gc`;
  server-authoritative remote maintenance.
