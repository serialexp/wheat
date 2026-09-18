# Wheat Native Protocol — Design

Status: partial — Phase 0 complete, Phase 1 standalone server + CLI client + e2e tests complete, remaining Phase 1–2 outstanding
Owner: bart
Last updated: 2026-09-19

## Implementation status

Tracking the gap between this design and what exists. The object format
(wheat-core) and local store (wheat-store) already exist; this document covers
the wire protocol, server daemon, and client synchronization layer built on
top of them.

### Done

- [x] **Object format and store abstraction.** `wheat-core` defines the content-
  addressed object model with binschema-generated encode/decode. `wheat-store`
  provides `ImmutableObjectStore` with local (`LooseObjectStore`) and S3-compatible
  (`S3ObjectStoreAdapter`) implementations, plus packed repositories, publication
  fencing, and the repository reader/mutator stack.
- [x] **Existing binschema object schema.** The wheat object wire format is already
  defined via binschema and generates Rust code (`wheat-core/generated/`). The
  protocol message schema is a second, separate schema.
- [x] **Phase 0 — bounded delta validation in wheat-store.** O(delta)
  `validate_transition` replaces the O(repository) full walk for publication.
  Delta methods added for `RetainedSetStore`, `ChangeMapStore`, and
  `ApplicationMapStore`; the existing `RevisionMapStore::delta` now has a
  production call site. Force-publish (`allow_non_monotonic`) bypasses
  monotonicity and lineage extension checks. Benchmarked at 500 baseline
  changes + 5 delta: 64ms (transition) vs 410ms (complete), 6.4× speedup.
  Tests: 6 retained-set delta tests, 4 transition validation tests, 1 bench.
- [x] **Phase 1 — standalone wheat server.** `wheat-server` binary with
  WebSocket listener, per-connection session setup (ServerHello → ClientHello →
  SessionEstablished), repository open with label manifest, iterative root-first
  fetch (ObjectBatch → ObjectWant loop → FetchComplete), content fetch with
  chunked streaming for large files, and publish with iterative BFS closure
  discovery (`extract_object_references`), content transfer, bounded delta
  validation, and CAS head advance. Registry supports single-repo (`--data-dir`)
  and multi-repo (`--repos`) modes. 3 integration tests (session setup, fetch,
  repo not found). Server helper `run_until()` supports in-process test use
  with port-0 binding and shutdown signaling.

- [x] **Phase 0 — protocol schema.** Defined at
  `crates/wheat-core/schemas/wheat-protocol.bs.json5`. 27 message types
  covering session setup, repository state, fetch (iterative root-first
  discovery), content transfer (batch + chunked + redirect), publication
  (request/objects/content/commit/progress/result), credit-based flow
  control, and errors with scope+retryable semantics.
- [x] **Phase 0 — Rust protocol crate.** `wheat-protocol` crate with
  binschema-generated encode/decode from the protocol schema, plus
  framing helpers (`decode_message`/`encode_message`) with size limit
  enforcement. Generation script at `scripts/wheat-protocol-gen-schema`.
  11 round-trip smoke tests verify all message families.
- [x] **Phase 1 — CLI client commands.** `wheat clone`, `wheat fetch`, and
  `wheat publish` in `wheat-cli`, speaking the native protocol over WebSocket
  to a `wheat-server` instance. URL scheme: `wheat://host:port/owner/repo`
  (TLS: `wheat+tls://`). Client protocol engine in `remote.rs` with iterative
  root-first fetch (BFS + retained-set walk for radix-trie-key ObjectIds),
  content fetch, and publish with ObjectsNeeded/ObjectBatch loop. Remote
  tracking via `remote.json` (url, last_fetched_generation, last_fetched_envelope)
  with divergence detection on fetch (refuses when local has unpublished work).
- [x] **Phase 1 — acceptance tests.** 3 end-to-end tests in
  `crates/wheat-server/tests/clone_fetch_publish_test.rs`: clone round-trip
  (validates complete structural closure via `validate_complete`), publish
  round-trip (clone → local change via `PersistedRepositoryMutator` → publish
  back → verify server head advanced), fetch after server advance (clone →
  server-side change → fetch update → verify new content). All tests exercise
  the full retained-set walk (RetainedSetStore cursor for views/revisions/
  applications/changes) and BFS through retained objects' children.

### Outstanding

- [ ] **Phase 1 — packed segments on the object store.** Extend the object store
  backend to write and read packed segments as S3 keys rather than one key per
  object. The server builds segments from incoming objects during publication and
  serves individual objects by index lookup and range read from segments. This is
  an economic requirement: per-request costs dominate per-byte costs for the small
  structural objects that make up most of a repository's object count.
- [ ] **Phase 1 — TypeScript client.** Generate TypeScript encode/decode from the
  same binschema schema, implement the client protocol over browser-native
  WebSocket, and demonstrate a round-trip from a Bun/browser client.
- [ ] **Phase 2 — remote tracking.** Persist observed remote views locally
  (`wheat-remote` configuration, remote-view observations) without treating them
  as local authority. Enable incremental fetch (only transfer objects the client
  is missing).
- [ ] **Phase 1 — authentication.** Bearer token auth in `ClientHello`, TLS
  required, localhost-only default when no auth is configured. Standalone
  server refuses non-loopback binding without explicit auth config. Origin
  validation for browser WebSocket connections.
- [ ] **Phase 2 — authorization and publish gates.** Server-side namespace/
  repository permission checks, read vs publish access control, and
  publish-gate enforcement (e.g. CI-required, review-required).
- [ ] **Phase 2 — Gothab integration.** Let gothab-server (or gitd's successor)
  serve wheat repositories through the native protocol alongside Git projections,
  with one canonical store per repository.
- [ ] **Open question — resumable transfer.** Whether large object transfers use
  chunked streaming within the WebSocket connection or out-of-band pre-signed URLs
  to the object store (relevant for R2/Hetzner deployments where the server could
  redirect rather than proxy).
- [ ] **Open question — migration path.** Whether existing Git-canonical repositories
  are imported once, dual-written, or lazily promoted. The existing `wheat import`
  is a one-shot bootstrap; the native protocol does not depend on answering this
  for Git repositories.

## Why this exists

Wheat has a complete local workflow: import a Git repository, create changes,
capture revisions, resolve conflicts, and push a projected view back to a Git
remote. The local model preserves stable change identity, immutable revisions,
deterministic views, and durable conflicts — none of which Git's ref/commit
model represents natively.

The problem is that wheat currently synchronizes exclusively through Git
projection. `wheat push` flattens a view into linear Git commits and publishes
one branch. `wheat import` ingests a Git repository into wheat objects. Both
operations are lossy: change identity, view composition, dependency graphs,
application evidence, and conflict history do not survive the round trip through
Git. A repository pushed from wheat and then imported back does not recover the
original wheat state.

This means wheat cannot be used collaboratively. Two developers with wheat
workspaces cannot share changes, views, or conflict resolutions with each other
except by projecting to Git and losing the information that makes wheat
different from Git.

The native protocol removes that bottleneck. It transfers wheat objects
directly — content-addressed, identity-preserving, and lossless — between
clients and servers. No Git projection, no identity fabrication, no information
loss.

## Goals

1. **Lossless native synchronization.** Clone, fetch, and publish transfer
   wheat objects with full fidelity. A repository cloned from a wheat server
   and then published back produces identical object identities.
2. **Open protocol.** The wire format and protocol semantics are specified
   independently of Gothab. A third party can implement a wheat server or client
   from the schema and this document alone. Git won because it was open; wheat
   should take that lesson.
3. **Object-store native.** The server stores objects in S3-compatible object
   storage (R2, Hetzner Object Storage) through the existing `ImmutableObjectStore`
   trait. Local filesystem is one backend, not the only one. Horizontal scaling
   does not require filesystem replication.
4. **Multi-language clients from one schema.** The protocol schema is a binschema
   `.schema.json`. Rust and TypeScript implementations exist from day one;
   additional languages (Go, Python, Zig) get generated encode/decode for free.
5. **WebSocket transport.** Binary protocol over WebSocket: bidirectional,
   works through proxies and CDNs, and enables browser-native clients without
   CLI installation.
6. **Thin server with bounded validation.** The server manages head pointers
   (generation + envelope ID), authentication, publication fencing, and
   bounded transition validation. Full-closure validation is prohibitively
   expensive for synchronous publication (757s on a 1,607-change repo); the
   server validates the *delta* — new objects are well-formed, the proposed
   envelope is a valid transition from the current head, and the closure is
   complete — without re-verifying the entire retained history. Composition,
   conflict detection, and view building remain client-side.

## Non-goals (v1)

- **Git protocol compatibility.** The native protocol does not speak the Git
  smart HTTP or pack protocol. Git interop remains `wheat import` and
  `wheat push` for v1; a compatibility adapter belongs to a later phase.
- **Server-side composition.** The server does not compose views, apply
  changes, or resolve conflicts. It is an authenticated object store with
  repository-head fencing. Composition happens on the client.
- **Replication and high availability.** A single server instance per
  repository is sufficient for v1. Multi-region replication and failover are
  future work enabled by the content-addressed object model (objects are
  immutable and safe to replicate).
- **Access control granularity.** v1 authenticates connections and gates
  publication. Per-change, per-view, or per-path authorization is deferred.
- **Delta encoding on the wire.** v1 transfers whole objects. Deltas reduce
  bandwidth but add complexity; the content-addressed model makes them
  optional (objects are typically small — edits, metadata, tree nodes — with
  large content bytes already stored separately by ContentId).

## Architecture

### Layers

```text
┌─────────────────────────────────────────────┐
│  wheat CLI / browser client                 │
│    wheat-protocol (generated encode/decode) │
│    wheat-store (ImmutableObjectStore)        │
│    wheat-core (model + object format)       │
└──────────────────┬──────────────────────────┘
                   │ WebSocket (binary frames)
┌──────────────────┴──────────────────────────┐
│  wheat-server                               │
│    wheat-protocol (generated encode/decode) │
│    wheat-store (ImmutableObjectStore)        │
│    wheat-core (object validation)           │
│    ┌──────────────────────────────────┐     │
│    │  backend: local / S3 / R2        │     │
│    └──────────────────────────────────┘     │
└─────────────────────────────────────────────┘
```

The client and server share the same crates. The protocol crate is the only
new dependency; both sides already have wheat-core and wheat-store.

### Trait-based server composition

The server is composed from pluggable trait implementations, following the
same pattern as `ImmutableObjectStore`. Every deployment-specific concern
is a trait the server takes as a parameter:

```text
WheatServer::new(
    object_store,      // ImmutableObjectStore — local / S3 / R2
    content_store,     // ContentBackend — global content tier
    head_coordinator,  // HeadCoordinator — CAS on repository heads
    catalog_backend,   // CatalogBackend — segment index / lookup
    auth_provider,     // AuthProvider — token validation
)
```

| Trait | Single-server | Gothab | S3-only |
|-------|---------------|--------|---------|
| `HeadCoordinator` | File lock (existing `publication.rs`) | Postgres row CAS | S3 conditional PUT |
| `CatalogBackend` | Local SQLite (existing `PackedCatalog`) | Postgres + S3 | S3 index object |
| `AuthProvider` | Config file / none (localhost) | Gothab auth middleware | OIDC / custom |
| `ImmutableObjectStore` | `LooseObjectStore` | `S3ObjectStoreAdapter` | `S3ObjectStoreAdapter` |
| `ContentBackend` | Local files | R2 / Hetzner | S3 |

The protocol crate defines only messages and framing — it knows nothing
about storage or coordination. The server crate composes the pieces. This
keeps the open-protocol promise: anyone can implement a wheat server by
providing these trait implementations against their own infrastructure.

### Crate layout

| Crate | Role |
|-------|------|
| `wheat-protocol` | Binschema-generated message types, WebSocket framing, client and server connection state machines |
| `wheat-server` | Standalone binary: WebSocket listener, repository registry, head management, auth |
| `wheat-cli` | Extended: `clone`, `fetch`, `publish` commands using `wheat-protocol` |
| `wheat-protocol-ts` | Generated TypeScript encode/decode from the same `.schema.json` |

### Object identity and transfer

Wheat objects are content-addressed: `ObjectId = blake3(canonical_bytes)`.
The canonical bytes are the binschema-encoded form defined in the existing
wheat object schema.

#### Fetch: iterative root-first discovery

A fresh clone cannot compute the transitive closure of an envelope it does
not have. The protocol uses **iterative root-first discovery**: the server
sends the root object (the envelope), the client decodes it, discovers
child references it is missing, requests those, decodes them, discovers
more references, and so on until the closure is complete. Each round is a
bounded batch.

```text
Client                          Server
  │                               │
  ├─ FetchRequest ───────────────>│  labels: ["main"], since: (gen, env)
  │                               │
  │<──── ObjectBatch ────────────┤  envelope + top-level objects
  │                               │
  │  (client decodes, discovers   │
  │   missing child ObjectIds)    │
  │                               │
  ├─ ObjectWant ─────────────────>│  [missing_ids...] (bounded)
  │<──── ObjectBatch ────────────┤  requested objects
  │                               │
  │  ... repeat until closure     │
  │  ... is complete ...          │
  │                               │
  ├─ FetchComplete ──────────────>│
```

For incremental fetch (client already has objects from a prior envelope),
the `since` field carries the last-observed `{generation, envelope}`. The
server can use this to compute the delta and send only new/changed
structural objects. If the server does not support delta computation, it
falls back to sending the full envelope and the client deduplicates
locally (objects it already has are skipped on receipt).

This avoids the chicken-and-egg problem: the client never needs to walk
a graph it doesn't have. The server sends roots, the client discovers
what's missing, and the protocol converges in O(graph depth) rounds with
bounded batches per round.

#### Batched transfer rules

- Each `ObjectBatch` is one WebSocket binary **message** (not frame —
  WebSocket messages may be fragmented into multiple frames by the
  transport; implementations work with reassembled messages).
- Maximum batch size: 4096 objects or 8 MiB of encoded payload, whichever
  is reached first.
- The client validates `blake3(bytes) == id` on receipt for every object.
  A mismatch is a fatal protocol error: the connection is closed with
  an error code identifying the corrupt object. The client does not
  silently retry or continue.

#### Content transfer

File content bytes (identified by `ContentId`) are transferred from the
global content store, separately from structural objects. The server
checks that the client's authorized repository graph references each
requested ContentId before serving it. Content is typically larger than
structural objects and benefits from different batching; direct-to-store
pre-signed URLs are viable here because content is stored as individual
keys (hash-prefix-sharded), not packed into segments.

Large content (multi-megabyte files) is streamed in bounded chunks rather
than sent as a single message, to respect WebSocket message size limits
and proxy constraints.

### Publication protocol

Publication is the write path: a client has composed a new view (with new
revisions, changes, edits, applications) and wants the server to accept it
as the new repository head. Publication is **asynchronous**: the client
submits objects and a proposed envelope, then the server validates and
commits (or rejects) independently. This avoids blocking a WebSocket
connection for the duration of validation.

```text
Client                          Server
  │                               │
  ├─ PublishRequest ─────────────>│  op_id, envelope_id,
  │                               │  expected_head: {generation, envelope}
  │                               │
  │                               │  server stores envelope, discovers
  │                               │  missing children iteratively
  │<──── ObjectsNeeded ──────────┤  op_id, [missing_ids] (bounded)
  │                               │
  ├─ ObjectBatch ────────────────>│  op_id, [(id, bytes), ...]
  │<──── ObjectsNeeded ──────────┤  op_id, [more missing_ids...]
  ├─ ObjectBatch ────────────────>│  op_id, [(id, bytes), ...]
  │                               │  ... repeat until server has
  │                               │  ... the full closure ...
  │<──── ObjectsComplete ────────┤  op_id (structural closure done)
  │                               │
  │<──── ContentNeeded ──────────┤  op_id, [missing content_ids]
  ├─ ContentUpload ──────────────>│  op_id, (id, bytes) or chunks
  ├─ ContentUploadComplete ──────>│  op_id (all content sent)
  │                               │
  ├─ PublishCommit ──────────────>│  op_id (client confirms commit)
  │                               │
  │                               │  server stages objects, runs
  │                               │  bounded delta validation:
  │                               │  - new objects well-formed
  │                               │  - envelope is valid transition
  │                               │  - closure complete
  │                               │  - CAS: generation == expected + 1
  │                               │
  │<──── PublishProgress ────────┤  op_id, phase, pct (optional)
  │<──── PublishResult ──────────┤  op_id, receipt_id,
  │                               │  status: ok | stale | rejected | error
  │                               │  new_head: {generation, envelope}
  │                               │
```

Every publish operation carries a client-generated `op_id` for connection-
scoped correlation and a `publish_key` (128-bit, client-generated, unique
per principal + repository) for cross-session idempotency. If the CAS
succeeds but the reply is lost, retrying with the same `publish_key`
returns the original receipt rather than `stale_head`. Retries with the
same `publish_key` but different envelope or expected head are rejected.
The server retains publish keys for a bounded window (default: 24h).

**Bounded delta validation.** The server does not re-verify the entire
retained history (757s on a representative repo). Instead it validates only
the *transition*:

- Every new object (not already in the store) has valid canonical encoding
  and `blake3(bytes) == id`.
- The proposed envelope references a valid state whose views, changes,
  revisions, and applications form a complete closure — all referenced
  ObjectIds exist in the store (pre-existing or newly uploaded).
- The transition from `expected_head.envelope` to the proposed envelope
  satisfies monotonicity: no retained changes/views are removed (append-only
  for non-GC transitions).
- Generation is exactly `expected_head.generation + 1`.

This is O(delta) not O(repository). A background audit can run full
`validate_complete` periodically without blocking publication latency.

**Staging and cleanup.** Objects uploaded during a publication attempt are
staged (not yet indexed in the catalog) until `PublishResult` confirms
success. If publication is rejected or the connection drops, staged objects
are garbage-collected after a TTL (default: 1 hour). Per-connection and
per-repository byte quotas bound the amount of staged data an attacker can
accumulate.

### Head management

The server owns exactly one mutable pointer per repository: a
`RepositoryHead = {generation: u64, envelope: RepositoryEnvelopeId}`.
This is the same structure used locally by `publication.rs`. The generation
counter is monotonically increasing and prevents ABA races: comparing only
envelope IDs would allow `A → B → A` to pass a stale CAS check.

Everything reachable from the envelope is immutable and content-addressed.
This makes the server's mutable state minimal:

- A repository registry: `{repo_id -> (head, object_store_config)}`
- Publication fencing: compare-and-swap on `{generation, envelope}`

The registry can be a database table, a config file, or an in-memory map
backed by a file — whatever suits the deployment. For Gothab integration,
it would be the existing repository row in Postgres. For multi-server
deployments, the CAS uses Postgres `UPDATE ... WHERE generation = $expected`
or an external coordination mechanism (Postgres row lock, S3 conditional
put, or a coordination service — deployment-specific, not a protocol
requirement).

### WebSocket framing

Each protocol message is a single WebSocket binary **message** containing:

```text
[message_type: u8] [binschema-encoded payload]
```

The message type byte selects which binschema union variant to decode. This
avoids a nested discriminator and lets implementations dispatch on the first
byte before entering the generated decoder.

**Terminology:** WebSocket messages may be fragmented into multiple frames
by the transport; implementations work with reassembled messages, not raw
frames. The protocol binds one record to one message.

**Size limits:** Maximum WebSocket message size is 16 MiB. Messages
exceeding this are protocol errors. WebSocket compression
(`permessage-deflate`) is disabled — compressed payloads can bypass wire-
size limits and the content is already binary/incompressible. Decoders
reject length-prefixed fields exceeding their type's bound *before*
allocating.

**Flow control:** TCP backpressure alone is insufficient — server-side
WebSocket libraries typically buffer outbound messages in userspace before
reaching the TCP send buffer. The protocol uses a credit-based flow
control scheme: the receiver advertises a **receive window** (in bytes
and message count) during session establishment. The sender pauses when
credits are exhausted and resumes when the receiver acknowledges durable
receipt. This bounds both server memory (during fetch streaming) and client
memory (during publication upload).

**Correlation IDs:** Every request carries a client-generated `op_id: u32`.
Every response includes the corresponding `op_id`. Concurrent operations
on one connection are permitted; the `op_id` disambiguates which response
belongs to which request. Implementations may enforce a maximum number of
concurrent operations (recommended: 4).

**Heartbeat:** Client and server exchange WebSocket ping/pong frames at a
configurable interval (default: 30s). A connection with no ping response
within 90s is considered dead and closed. This prevents silent proxy/CDN
idle disconnects.

### Connection lifecycle

```text
Client                          Server
  │                               │
  ├─ Connect (WebSocket + TLS) ──>│
  │                               │
  │<──── ServerHello ────────────┤  version_min, version_max,
  │                               │  server_id, capabilities
  ├─ ClientHello ────────────────>│  selected_version, auth_token,
  │                               │  receive_window
  │                               │
  │<──── SessionEstablished ─────┤  selected_version, receive_window,
  │                               │  status: ok | version_mismatch |
  │                               │          auth_failed
  │                               │
  ├─ RepositoryOpen ─────────────>│  repo_name or repo_id
  │<──── RepositoryState ────────┤  head: {generation, envelope},
  │                               │  labels: [{name, view_id}, ...]
  │                               │
  │  ... fetch / publish / list ...
  │                               │
  ├─ Close ──────────────────────>│
```

**Version negotiation:** `ServerHello` advertises a version range.
`ClientHello` selects one version within that range. `SessionEstablished`
confirms the selected version or rejects with `version_mismatch` and the
server's supported range. The selected version governs which message types
and field layouts are valid for the session.

**Authentication:** Connections require TLS. The `auth_token` field in
`ClientHello` carries a bearer token. The server validates the token and
binds the session to a principal (user or service account). Unauthenticated
connections are rejected with `auth_failed`. The standalone v1 server
defaults to **localhost-only binding** (127.0.0.1) when no auth
configuration is provided; binding to non-loopback addresses requires
explicit auth configuration. This prevents accidentally exposing an open
server.

**Origin validation:** Browser WebSocket connections include an `Origin`
header. The server validates it against an allowlist to prevent cross-site
WebSocket hijacking.

**Labels enumeration:** `RepositoryState` includes the full label manifest
(label name + view ID pairs), not just a count. This lets the client
discover branches by name without a separate round trip. For repositories
with many labels, the response is bounded (max 10,000 labels; the server
truncates with a flag indicating more are available, and a separate
`ListLabels` paginated message can retrieve the full set).

## Wire protocol schema (sketch)

The actual schema is at `crates/wheat-core/schemas/wheat-protocol.bs.json5`.
The sketch below records the original design intent; refer to the `.bs.json5`
file as the source of truth. This sketch shows the message shapes. All arrays have explicit maximum item counts and byte limits;
strings have maximum byte lengths. Binschema generates the encode/decode
logic; the **framing layer** (handwritten, wrapping the generated code)
enforces aggregate message-size bounds and per-field maximums before
allocating. If binschema gains native constraint support, the framing
checks become redundant but harmless.

```
// --- Session ---

ServerHello
  protocol_version_min: u16
  protocol_version_max: u16
  server_id: bytes[32]
  capabilities: array<string, max=64>         // extensible feature flags

ClientHello
  selected_version: u16
  auth_token: bytes (length-prefixed, max 8 KiB)
  receive_window_bytes: u32                   // flow control
  receive_window_messages: u16

SessionEstablished
  selected_version: u16
  receive_window_bytes: u32
  receive_window_messages: u16
  status: enum { ok, version_mismatch, auth_failed }
  supported_range_min: u16                    // included on mismatch
  supported_range_max: u16
  message: string (max 1 KiB, optional)

// --- Repository ---

RepositoryOpen
  name: string (max 256 bytes)

RepositoryState
  head_generation: u64
  head_envelope: bytes[32]
  labels: array<{ name: string (max 256), view_id: bytes[32] }, max=10000>
  labels_truncated: bool

ListLabels
  op_id: u32
  cursor: bytes (max 256, empty = start)

ListLabelsResult
  op_id: u32
  labels: array<{ name: string, view_id: bytes[32] }, max=10000>
  next_cursor: bytes (max 256, empty = end)

// --- Fetch (iterative root-first discovery) ---

FetchRequest
  op_id: u32
  labels: array<string, max=64>              // which labels to fetch
  since_generation: u64                       // 0 = full clone
  since_envelope: bytes[32]

ObjectBatch
  op_id: u32
  sequence: u32                               // batch ordering
  objects: array<{ id: bytes[32], data: bytes (max 1 MiB) }, max=256>
  // Aggregate encoded size must not exceed 8 MiB (enforced by sender,
  // within the 16 MiB WebSocket message cap). Structural objects are
  // typically under 1 KiB; the 1 MiB per-item max covers outliers.

ObjectWant
  op_id: u32
  ids: array<bytes[32], max=4096>             // bounded per round

FetchComplete
  op_id: u32

// --- Content ---

ContentFetchRequest
  op_id: u32
  content_ids: array<bytes[32], max=4096>

ContentBatch
  op_id: u32
  sequence: u32
  contents: array<{ id: bytes[32], data: bytes (max 4 MiB) }, max=4>
  // Aggregate encoded size must not exceed 12 MiB (headroom within
  // the 16 MiB WebSocket cap for framing overhead). Large content
  // (> 4 MiB) is transferred as multiple ContentChunk messages
  // rather than a single ContentBatch item.

ContentChunk                                  // streaming large content
  op_id: u32
  content_id: bytes[32]
  offset: u64                                 // byte offset in the file
  data: bytes (max 4 MiB)
  final: bool                                 // last chunk for this id

ContentRedirect                               // server offers pre-signed URL
  op_id: u32
  content_id: bytes[32]
  url: string (max 4 KiB)
  expires_seconds: u32

// --- Publish ---

PublishRequest
  op_id: u32
  publish_key: bytes[16]                      // cross-session idempotency
  envelope_id: bytes[32]
  expected_generation: u64
  expected_envelope: bytes[32]
  force: bool                                 // skip monotonicity check

ObjectsNeeded
  op_id: u32
  ids: array<bytes[32], max=4096>

ObjectsComplete                               // server has full closure
  op_id: u32

ContentNeeded                                 // server lists missing content
  op_id: u32
  content_ids: array<bytes[32], max=4096>

ContentUpload                                 // client pushes content bytes
  op_id: u32
  content_id: bytes[32]
  data: bytes (max 4 MiB)                     // small content in one message

ContentUploadChunk                            // client pushes large content
  op_id: u32
  content_id: bytes[32]
  offset: u64
  data: bytes (max 4 MiB)
  final: bool

ContentUploadComplete                         // all content uploaded
  op_id: u32

PublishCommit
  op_id: u32

PublishProgress
  op_id: u32
  phase: string (max 64)
  percent: u8                                 // 0-100, 255 = indeterminate

PublishResult
  op_id: u32
  receipt_id: bytes[16]                       // idempotency key
  status: enum { ok, stale_head, rejected, already_committed, error }
  new_generation: u64
  new_envelope: bytes[32]
  message: string (max 4 KiB, optional)

// --- Flow control ---

Credit
  op_id: u32                                  // which operation this ack
                                              // applies to (0 = global)
  ack_through_sequence: u32                   // acknowledges all batches
                                              // up to this sequence for
                                              // the specified op_id
  window_bytes: u32                           // additional bytes the
                                              // receiver can accept
  window_messages: u16                        // additional messages
  // Credit is per-operation: each concurrent operation has its own
  // sequence space and credit window. op_id=0 is reserved for
  // connection-level credit (e.g. during session setup before any
  // operation is opened). Sequence numbers are scoped to (op_id,
  // direction) — ObjectBatch.sequence in op A and op B are independent.
  //
  // Credit, Error, and session-level messages (Hello, SessionEstablished,
  // Close) are exempt from flow control to avoid deadlock.
  // Window values are ADDITIVE: each Credit adds to the sender's
  // remaining budget for that operation. The ack_through_sequence field
  // serves as a durable-receipt acknowledgment: the sender may discard
  // retry state for batches at or below this sequence in this operation.

// --- Errors ---

Error
  op_id: u32                                  // 0 = connection-scoped
  code: u16
  scope: enum { connection, operation }
  retryable: bool
  message: string (max 4 KiB)
  object_id: bytes[32] (optional)             // which object failed
```

### Message type allocation

Message type bytes are append-only: once a type byte is assigned in a
released protocol version, it is never reassigned or removed. New message
types are added at the end. Unknown message types received by a peer are
logged and ignored (not a fatal error) — this allows a newer server to
send capability-gated messages to older clients without breaking them.

### Schema evolution policy

**The protocol schema uses a different encoding discipline than the object
schema.** The object schema (`wheat-core::wire`) is strict-canonical:
`decode_validated` rejects trailing bytes. The protocol schema must be
**extensible**: decoders skip unknown trailing bytes within a message
rather than rejecting them. This allows field addition without breaking
older peers. Binschema supports both modes; the protocol schema must
explicitly opt into extensible encoding.

The protocol schema version is incremented when:
- A new message type is added (minor bump, backward compatible — older
  peers ignore unknown types in the designated extensible range)
- A field is added to an existing message (minor bump — older decoders
  skip the trailing unknown bytes)
- A field is removed, reordered, or has its type changed (major bump,
  requires version negotiation to exclude older peers)
- A message type's semantics change (major bump)

Unknown message types are only safe to ignore if they are in a designated
"extension" range (type bytes 128-255). Types 0-127 are core protocol
messages; receiving an unknown type in the core range from a peer at the
negotiated version is a protocol error.

The object schema (`wheat-core`) and the protocol schema evolve
independently. The protocol carries supported object schema versions in
`ServerHello.capabilities` so the client can detect incompatible object
formats before fetching.

## Server deployment model

### Standalone

```text
wheat-server \
  --listen 127.0.0.1:7420 \
  --store s3 \
  --endpoint https://s3.eu-central-1.amazonaws.com \
  --bucket wheat-repos \
  --registry ./repos.json \
  --auth-config ./auth.json
```

The standalone server is a single binary. It reads repository configuration
from a registry file or environment and serves over WebSocket. This is the
"anyone can run it" deployment. It defaults to localhost-only binding;
non-loopback addresses require explicit `--auth-config`.

### Embedded in Gothab

Gothab mounts the wheat protocol handler on a route (e.g. `/wheat/ws`) behind
its existing authentication middleware. The repository registry is the Postgres
`repositories` table. Object storage uses the existing blobstore configuration
per namespace. No separate binary — the protocol handler is a library called
from gothab-server.

### Object store as the scaling axis

The server is stateless except for the head pointer. All objects live in the
object store. This means:

- **Multiple servers can share one bucket.** Objects are content-addressed and
  put-if-absent; concurrent writes to the same object are safe.
- **The server uses local disk only for scratch.** Segment construction
  during publication and verification scratch during validation require
  temporary local storage, but the durable state lives in the object store.
  Scratch is bounded and cleaned up on completion or timeout.
- **Direct-to-store reads (future).** For large content, the server could
  issue pre-signed URLs and let the client read from the object store directly,
  bypassing the server for data-plane bytes while keeping the control plane
  (negotiation, publication) on the WebSocket.

### Packed segments on the object store

The local store packs many small objects into self-authenticating segments
(up to 1M entries, 128 MiB payload per segment) to reduce filesystem overhead.
The current S3 adapter (`ObjectStoreAdapter`) stores each object as an
individual key (`prefix/ab/cdef...`), which is simple but economically
untenable at scale. A typical repository has tens of thousands of small
structural objects (tree nodes, revision records, view records, edit
definitions — most under 1 KB). At one S3 key per object:

- **Clone = 50k GET requests.** Each costs money. The per-request cost
  dwarfs the per-byte storage cost for sub-KB objects.
- **Publish = hundreds of PUT requests** per revision, for tree nodes and
  metadata that could share a single key.
- **Cost scales with request count, not data volume.** A 50 MB repository
  spread across 50k keys costs far more to serve than a 500 MB repository
  packed into 50 segments.

Packed segments on the object store are therefore an economic requirement,
not an optimization. The segment format already has the right properties for
remote storage: self-authenticating (body digest), indexed (sorted pages with
binary-searchable directory), range-readable (known offsets), and immutable
once sealed. A clone becomes a handful of large S3 GETs instead of tens of
thousands of small ones.

This has direct implications for the transfer model:

- **Server-mediated reads are the default path.** The server reads from the
  object store (potentially from packed segments) and streams individual objects
  to the client. This is always correct regardless of physical layout.
- **Direct-to-store pre-signed URLs work only for large content.** File content
  bytes (identified by `ContentId`) are typically large enough to justify their
  own S3 key and can be served via pre-signed URL. Small structural objects
  (trees, revisions, views, edit definitions) are packed into segments and
  require server-side extraction — a pre-signed URL to a segment is useless
  without the segment index.
- **Segment-level pre-signed URLs with client-side extraction.** An alternative
  is to send the client the segment index and a pre-signed URL to the whole
  segment, letting the client do range reads. This requires the client to
  understand the segment format, which is additional complexity but reduces
  server bandwidth. Worth considering for bulk clone of large repositories.

The protocol is agnostic to the server's physical layout. The client
requests objects by ID; the server decides how to serve them. Whether the
server reads from loose keys, packed segments, or a local cache is an
implementation detail behind `ImmutableObjectStore`.

**Segment-aware fetch planning.** To actually realize the request-count
savings, the server must group wanted object IDs by segment, then read each
relevant segment once (or coalesce range reads within a segment) rather
than performing individual object extractions. A bounded in-memory segment
cache avoids re-reading hot segments across concurrent clients. Without
this, a 50k-object clone could still produce tens of thousands of S3 range
reads despite packed storage.

**Remote catalog design.** The existing `PackedCatalog` is process-exclusive
(local SQLite + file lock). A remote object store backend requires a
distributed catalog:

- **Object-to-segment index** stored as a well-known key in the bucket
  (or as a separate S3 object with conditional-put versioning).
- **Multi-writer segment sealing** uses a coordination mechanism (S3
  conditional put, database row lock, or external lock service) to
  ensure only one server instance seals a given segment. A single-server
  deployment needs no coordination — process-local locking suffices.
- **Atomic publication** of segment + index entry: seal the segment to
  S3, then atomically update the index. If the index update fails, the
  orphan segment is cleaned up by GC.
- **Cache invalidation** across server instances: index version is checked
  on each operation; a stale cache triggers a re-read of the index.

### Two-tier storage: global content, per-network structural objects

Storage is split into two tiers with different dedup scopes and access rules:

**Tier 1 — global content store (file bodies, by ContentId).** One
instance-wide store, sharded by content hash prefix (e.g.
`content/ab/cdef...`). A file uploaded by any repository is stored once,
globally. Two completely unrelated repositories with the same file share
the same bytes without needing to be in the same fork network.

Content is access-gated, not access-open. The server serves content only
to clients whose authorized repository graph references that ContentId —
knowing a hash is not sufficient to download the bytes. This is the same
model Gothab already uses for git blob ownership
(`docs/design/repository-blob-ownership.md`): content exists once
physically, but authorization is checked against the requesting
repository's tree.

**Tier 2 — per-fork-network structural store (trees, revisions, views,
edits, by ObjectId).** Each fork network has its own set of packed
segments. A fork is a new head pointer into the same structural object
pool. Creating a fork copies zero bytes.

```text
instance-wide content store
  content/ab/cdef...    <- stored once, even if 50 repos reference it
  content/xy/zabc...

fork network "serialexp/gothab"
  segments/             <- structural objects (trees, revisions, views)
    aabb...segment
    ccdd...segment
  heads/
    serialexp/gothab    -> envelope aaa...
    alice/gothab        -> envelope bbb...

fork network "other/project"
  segments/
    eeff...segment
  heads/
    other/project       -> envelope ddd...
```

This split cleanly separates the two concerns:

- **Authorization** lives in the structural tier. The tree structure tells
  you which files exist in a repository and where — that is the sensitive
  information. Structural objects are fork-network-scoped and auth-gated
  per repository within the network (a client authenticated to fork A
  cannot read structural objects only reachable from private fork B).
- **Dedup** lives in the content tier. Same bytes = same hash = stored
  once, instance-wide. No fork-network coordination needed for content
  dedup. The authorization check happens at the structural layer above.

**Garbage collection** operates differently per tier and must handle
concurrent publication, fork changes, and active readers:

- **Structural GC** walks all heads in a fork network, computes reachable
  objects, and removes segments containing only unreachable objects.
  Scoped to one fork network — independent of other networks. GC
  snapshots heads under a generation barrier: publications that land
  after the snapshot contribute to the *next* GC cycle, not the current
  one. Segments are marked for deletion, then physically removed after
  a grace period (default: 24h) to allow in-flight reads and cached
  locators to drain. Compaction (replacing a partially-dead segment
  with a smaller one) publishes the new segment and updates the index
  atomically before marking the old segment for deferred deletion.
- **Content GC** sweeps the global content store against content manifests
  across all fork networks. A ContentId is collectible only when no
  structural object in any network references it. This is bounded: walk
  content manifests (compact sets of ContentIds), not full object graphs.
  Same grace period applies — content may still be referenced by
  pre-signed URLs or in-flight fetch operations.
- **Staged-object cleanup.** Objects uploaded during a rejected or
  abandoned publication are garbage-collected after TTL (1h). A
  per-connection byte quota bounds the staged data an unauthenticated
  or misbehaving client can accumulate.

GC is disabled by default in v1 and must be explicitly enabled once
the generation barrier, grace periods, and fork-network head snapshots
are qualified.

## TypeScript client

The TypeScript client is generated from the same binschema `.schema.json` and
runs in browser or Bun:

```typescript
import { WheatClient } from 'wheat-protocol';

const client = await WheatClient.connect('wss://wheat.example.com/repo/main');
const state = await client.repositoryState();

// Fetch all objects since our last observation
const objects = await client.fetch({ since: lastKnownEnvelope });

// Publish a new view
const result = await client.publish({
  envelope: newEnvelopeId,
  expectedHead: state.envelopeId,
  objects: newObjects,
});
```

The generated code handles encode/decode; the client library adds WebSocket
lifecycle, batching, and error handling. This is enough to build:

- A browser-based repository viewer that reads wheat objects directly
- A Gothab frontend feature that displays wheat-native views
- A CI integration that publishes wheat revisions without the Rust CLI

## Phasing

### Phase 0 — foundations

1. Build bounded delta validation in `wheat-store` — verify only the
   transition, not the full retained history. This is a prerequisite for
   usable publication latency.
2. Define the protocol `.schema.json` with all message types, bounds, and
   correlation IDs. Generate Rust types.
3. Implement WebSocket framing with size limits and credit-based flow control.
4. Unit-test encode/decode round-trips and byte-level compatibility fixtures.
5. Define schema evolution policy (append-only type allocation, version
   negotiation rules, unknown-message behavior).

### Phase 1 — server, CLI client, TypeScript client, and auth

1. Build the standalone `wheat-server` with local and S3 backends,
   including segment-aware fetch planning and bounded segment cache.
2. Implement bearer-token authentication in `ClientHello`. Standalone
   server defaults to localhost-only without auth config.
3. Add `wheat clone`, `wheat fetch`, and `wheat publish` to the CLI,
   using iterative root-first discovery for fetch.
4. Generate the TypeScript client from the same binschema schema.
   Demonstrate a cross-language round-trip over WebSocket.
5. End-to-end acceptance tests with an ephemeral server: clone, fetch,
   publish, cross-language fidelity, mid-transfer disconnect, stale-head
   rejection, malformed-object rejection, unauthenticated rejection.

### Phase 2 — remote tracking, authorization, and Gothab integration

1. Persist remote-view observations in the local store. `wheat fetch`
   transfers only new objects (incremental sync via `since` generation).
2. Server-side authorization: read vs publish permissions, namespace/
   repository access control, publish-gate enforcement.
3. Fork-network structural-object reachability (see open question 4).
4. Integrate with gothab-server: mount protocol handler on `/wheat/ws`,
   use Postgres repository registry and existing blobstore config.

### Phase 3 — production hardening

1. Resumable transfers, direct-to-store content reads (pre-signed URLs).
2. Remote packed catalog with multi-writer coordination (coordinated
   segment sealing, distributed index).
3. Rate limiting, connection limits, per-repository byte quotas.
4. Server-side garbage collection with generation barriers and grace
   periods.
5. Monitoring, health/readiness endpoints, audit logging, tracing IDs.
6. Graceful draining, keepalive tuning, load shedding, proxy/CDN
   compatibility testing.

## Resolved design questions

All initial open questions have been settled:

- ~~**Content vs structural objects.**~~ Content (file bodies by ContentId)
  lives in a global instance-wide store, sharded by hash prefix. Structural
  objects (trees, revisions, views by ObjectId) live in per-fork-network
  packed segments. Separate message types for each tier.

- ~~**Product boundary.**~~ Open protocol, not internal substrate.

- ~~**Auth phasing.**~~ Auth is Phase 1 (not Phase 2). Standalone server
  defaults to localhost-only when no auth is configured.

- ~~**Resumable large transfers.**~~ Nontrivially large content is stored
  chunked (following Gothab's existing chunked-upload pattern). Transfer
  resumes from the last successfully received chunk. Structural objects are
  small enough that retry-from-scratch is acceptable; content is where
  resumability matters.

- ~~**Structural object access.**~~ The server gathers and proxies structural
  objects from packed segments, with a bounded server-side cache for
  frequently accessed segments. No client-side segment parsing required.
  For content, the server may cache pre-signed URLs and serve those to the
  client instead of proxying bytes — getting the bandwidth savings of direct
  access without exposing storage layout.

- ~~**Multi-server head coordination.**~~ The reference `HeadCoordinator`
  implementation uses S3 conditional PUT, since an S3-compatible object store
  is always available when using the remote backend — zero extra
  infrastructure. The trait allows other implementations (Postgres row CAS,
  etc.) for deployments that have them.

- ~~**Content reachability across forks.**~~ Maintain an append-only set of
  ContentIds that have ever been referenced by a repository. This set only
  grows — content added is never removed from the set, even if the tree that
  referenced it is later replaced by a force-publish. Access check is a
  membership test against this set: O(1) with a bloom filter or hash set,
  no graph walk required. Every publication appends the new content
  manifest's entries. This is much cheaper than per-read reachability walks
  and avoids the information-disclosure problem where a client could probe
  for ContentIds belonging to a different private fork.

- ~~**Token expiry during long transfers.**~~ The server issues an operation
  token at session establishment, refreshed via the WebSocket keepalive
  mechanism. Once a session is authenticated, it stays authenticated for the
  duration — the operation token represents "this connection was
  authenticated and is still alive." If the keepalive lapses (no refresh
  within the timeout), the session is terminated. No mid-transfer
  re-authentication dance.

- ~~**Force-publish / publish policy.**~~ The protocol supports both
  monotonic (append-only, default) and force-publish (non-monotonic, explicit
  flag) transitions. Force-publish allows removing retained changes,
  rewriting labels, and non-descendant transitions — necessary for secret
  removal and repository administration. Force-publish requires elevated
  authorization (server-side permission check) and is logged/audited. Delta
  validation enforces monotonicity by default; force-publish skips the
  monotonicity check but still validates object well-formedness and closure
  completeness.

## References

- `docs/design/server-native-source-control-store.md` — the parent design
  recording the broader dilemma of forge-first vs substrate-first and the
  existing `gothab://` seam.
- `docs/design/wheat-local-cli.md` — the local CLI design, including Phase 3
  synchronization placeholders that this document supersedes.
- `docs/design/repository-blob-ownership.md` — the existing git-blob
  ownership model; the content-reachability design above is the wheat analog.
- `crates/wheat-core/generated/` — the existing binschema-generated object
  format (Rust).
- `crates/wheat-store/src/publication.rs` — `RepositoryHead` with
  `{generation, envelope}` CAS, the model for the wire head representation.
- `~/Projects/binschema` — the binschema schema definition and code generation
  tool.
- `crates/wheat-store/src/loose.rs` — `ImmutableObjectStore` trait definition.
- `crates/wheat-store/src/object_store_adapter.rs` — S3-compatible object store
  implementation.
