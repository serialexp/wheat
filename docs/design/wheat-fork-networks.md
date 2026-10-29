# Wheat fork networks — Design

Status: partial — phase 0 (networks in the registry and on disk), the directory
fork, network copy and phase 1 for the standalone server (content apart) landed;
Gothab's opener and the follow-ups below outstanding. Decided (Bart,
2026-10-27): one structural pool per fork network, one instance-wide content store,
a hash is a read capability, publishes upload only what the store lacks, a fork
starts with all its parent's labels, and the standalone server keeps networks in
network directories.
Owner: bart
Last updated: 2026-10-29

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they land
and move them from Outstanding to Done.

### Done

- [x] **Decision — structure (2026-10-27).** One packed structural pool per fork
  network; a fork is a new head in its network's pool (option F2 of three: copy on
  fork, pool per network, layered stores).
- [x] **Decision — content (2026-10-27).** One content store for the whole instance
  (option C3 of three: per repository, per network, instance-wide).
- [x] **Decision — fetch (2026-10-27).** Open question 1, option C: a hash is a
  capability. See [Decisions](#decisions-2026-10-27).
- [x] **Decision — publish (2026-10-27).** Open question 2, option B: upload only
  what the store lacks.
- [x] **Decision — labels (2026-10-27).** Open question 3: a fork starts with all of
  its parent's labels, so creating one writes nothing to the pool.
- [x] **Decision — standalone layout (2026-10-27).** Network directories with a
  name index (option L1 of three: network directories, forks pointing at their
  root's directory, no directory forks). See [Layout](#layout).
- [x] **Phase 0 — networks in the registry.** `RepositoryEntry::network` is a
  `wheat_server::network::Network`, one shared handle per network to its store and
  its writers, so `Registry::quiesce` on any repository holds the whole network.
  Openers share handles through `OpenNetworks`, which opens a network's store with
  its first repository and closes it with its last (waiting out a close in progress
  before opening again). `DirectoryOpener` reads `wheat_store::RepositoriesDirectory`.
- [x] **Phase 4, standalone — forking in a repositories directory.**
  `RepositoriesDirectory::fork` writes a head naming the parent's envelope and a
  name; nothing is written to the store (`fork_network` bench case).
- [x] **Phase 5 — copying a network.** `NetworkDirectory::files` and `copy_to` list
  and copy the store and every head; the copy loop is `RepositoryDirectory`'s,
  which `repository_copy` measures.
- [x] **Phase 1, standalone — the content tier apart.** A repositories directory
  keeps every network's content in one packed store, `<repos-dir>/content`
  (`RepositoriesDirectory::open_content`; Bart chose a packed store over a loose
  directory, 2026-10-27), which `DirectoryOpener` opens once and holds, so one
  process serves a directory. `wheat_store::NetworkStore` pairs it with each
  network's pool: objects go to the pool and content to the content store, each in
  one durable append of its own. That doubles a group commit's write, about 3.0 ms to
  6.0 ms in the `network_publish` bench case; Bart accepted that over running the two
  appends at once (measured at 1.7x), 2026-10-27. `remote_fork` (wheat-cli) clones a
  fork made while its parent is served.
- [x] **Adopting a standalone repository (2026-10-28).**
  `RepositoriesDirectory::adopt_repository` copies a `RepositoryDirectory` into a
  network of its own with new IDs, adds its content to the content store, validates
  it and installs the network before writing the name (`network_import.rs`; see
  `wheat-network-maintenance.md`).

- [x] **Moving a standalone network (2026-10-29).** A bundle carries the pool, the
  heads and the content they reach: `wheat admin export`, `import` on the other
  server, then `delete` here (`wheat-network-maintenance.md`, Phases 0–3).
- [x] **Deleting a fork while its network is served (2026-10-29).** Removing a head
  no longer needs the network's store closed; only the network's last repository
  does. `Registry::delete` ends the sessions on the repository and waits for its
  holders before deleting (`wheat-network-maintenance.md`, Phase 3).

### Outstanding

- [ ] **Phase 1 and 4, Gothab — its opener.** A host whose database keeps heads
  creates a fork's head as `RepositoryHead::initial(parent envelope)`, maps names to
  networks, and pairs each network's pool with its blob store
  (`SeparateRepositoryStore` over `ContentStore::over_blobs`), sharing pools through
  `OpenNetworks`. Nothing in Wheat is missing for it yet; revisit when Gothab's
  opener exists.
- [ ] **Defer — network GC.** Marking every head in a network and every network for
  content is the open `wheat gc` / server-authoritative maintenance work in
  `TODO.md`. Deleting a network leaves its content in the content store until then.
- [ ] **Superseded — phases 2 and 3.** Reads limited to the repository's closure
  and publishes proving possession are not needed after the 2026-10-27 decisions.
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

As built (`wheat_store::RepositoriesDirectory`, 2026-10-27), networks and their
repositories are named by UUIDs, and a name record says where a repository is, so a
rename touches only the name:

```text
<repos-dir>/
  names/<owner>/<repo>                    RepositoryLocationV1: network and repository
  networks/<network>/
    storage/
    heads/<repository>/repository.head
```

A new network is built in a hidden stage beside its name and renamed into
`networks/`; a deleted head or network is renamed to a tombstone beside its name
first. Both are swept under the name's lock, as `RepositoryDirectory` sweeps its own.
The name is written last and removed last, so a crash leaves a network or head no
name refers to (for network GC) or a name whose head is gone, which counts as no
repository and is replaced by the next create or fork of the name. The content store
is not there yet (phase 1).

Phase 1 added it, `<repos-dir>/content/`, a packed store whose objects side is
unused; a network's `storage/` then holds objects only.

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
- Publish, standalone: a group commit's write is two durable appends, the network's
  pool and the content store, one after the other (`network_publish` bench case).
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

After the decisions below, phases 2 and 3 fall away: phase 4 follows phases 0 and 1.

## Decisions (2026-10-27)

**Fetch: a hash is a capability (option C).** Bart's reasoning: an object's ID is
the hash of its bytes, and it appears only inside the objects that refer to it, so a
client that can name an object has already been given something that refers to it.
Nobody found a practical way to learn a private fork's hash without that fork's
content. The two cases looked at, neither of which reveals anything new:

- Someone who lost access to a fork keeps the hashes they saw, and the bytes too.
- A client that guesses a blob's exact bytes can hash them and ask whether the
  instance holds them. It learns that the blob exists somewhere, not where, and it
  had the bytes already. Publishing (below) answers the same question.

So fetch serves any object of the network's pool and any blob of the content store,
as it does today, and the rule in `wheat-native-protocol.md` that a client of fork A
cannot read objects only reachable from private fork B is withdrawn. "What sharing
breaks today" above is the analysis this decision answers.

**Publish: only what the store lacks (option B).** By the same reasoning, a client
naming an object or blob it does not send must have had it, so the server asks for
what the store lacks, as today, and no repository-level check is added.

**Labels: all of the parent's.** A fork's head names the parent's envelope.

## References

- `docs/design/wheat-native-protocol.md`, "Two-tier storage" and its resolved
  questions.
- `docs/design/wheat-storage-efficiency.md`, cross-repository sharing and GC scope.
- `TODO.md`, "Hosting on Gothab": Forks; content fetch reachability; `wheat gc`;
  server-authoritative remote maintenance.
