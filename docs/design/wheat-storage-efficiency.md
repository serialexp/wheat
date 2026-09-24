# Wheat Storage Efficiency — Design

Status: partial — Phases 0–2 and local Phase 4a landed; Phase 3, remote maintenance, and Phase 4b outstanding
Owner: bart
Last updated: 2026-09-24

## Implementation status

Tracking the gap between this design and what exists. The analysis
comes from profiling the object closure of a 50-change benchmark
repository against the equivalent git representation. Object counts
and byte sizes need real measurement before implementation — the
estimates here are back-of-envelope and several were corrected after
external review.

### Done

- [x] **Phase 0 — content delta compression.** Delta codecs 2 (delta)
  and 3 (delta+zstd) added to the content envelope. Delta encoding uses
  `gunnar-delta` (pure-Rust git pack delta format); decoding inlines the
  gix-pack apply logic. Content store transparently resolves delta chains
  up to depth 16. Server publish path diffs old/new Merkle trees to find
  delta bases. Commit writer passes base hints for generated content.
- [x] **Phase 1 — collection compaction.** Radix trie nodes now inline
  single-entry leaf subtrees as entries (key suffix + value) in the parent
  node body. Reduces node count by ~70-85% for typical collections while
  preserving Merkle identity, O(Δ) `paired_diff`, and O(log N)
  incremental COW updates. All 7 collection types updated: wire format,
  builders, cursor, get, update_batch/update_node, paired_diff,
  context_walk, paired_context_walk, snapshot closure walker,
  extract_object_references, content accumulator (import path).

- [x] **Phase 1.5 — flat sorted blob content sets.** Replaced the radix
  trie content set representation (ContentSetNodeV1 objects) with flat sorted
  blob snapshots (ContentSetSnapshotV1) and delta chains
  (ContentSetDeltaV1). Full snapshot every 16 views, compact deltas in
  between. Reduces content set storage from 348 MiB to ~14 MiB on the
  Gothab benchmark (96% reduction). ContentSetStore rewritten with
  resolve/write_snapshot/write_delta operations backed by sorted Vec
  operations instead of radix tree traversal. All consumers updated:
  commit_content, commit_writer, repository_mutation, content_accumulator,
  repository_verify (visible_set), repository_hydration,
  repository_transition, canonical_closure, snapshot walker, staged_import.

- [x] **Phase 2 — segment-level block compression.** Small metadata
  records (<1024 bytes) are grouped into ~64 KB blocks and compressed
  as single zstd frames (codec=4 "ZstdBlock"). Multiple index entries
  share the same `(offset, stored_length)` — the reader decompresses
  the block and extracts records at precomputed within-block offsets.
  No segment version bump (V1 format, new codec byte). SegmentBuilder
  and BulkSegment buffer small records and flush blocks at the target
  size or at seal time. seal_active() rebuilds with block compression
  instead of raw-copying. SQLite locator schema updated for codec=4.
  50-change benchmark: 205 KiB stored vs 288 KiB uncompressed (29%
  savings).
- [x] **Phase 4a — nondestructive local repack.** `wheat repack` converts or
  rewrites an existing local packed/hybrid store through one authenticated V2
  physical-catalog generation. Structural records stream through bounded segment
  builders; content uses one sharded generation with reverse deltas, every retained
  lineage tip full, and depth capped at 16. Planning is read-only, capacity is
  confirmed before replacement bytes are produced, publication binds the exact
  repository and physical authorities, recovery resolves to the complete old or
  new generation, and a canonical second run is a true no-op. Loose stores and
  shared remote backends are refused rather than silently migrated or swept.
- [x] **Phase 4a+ — metadata history deltas.** Repack now stores older
  metadata (per-view ContentSetSnapshots, Trees, RevisionMap and
  ApplicationMap nodes) as git-delta patches against the newer version of the
  same object along the view lineage, inside segments. Loose metadata is
  absorbed into segments and its files are retired after the new head commits.
  See [Metadata history deltas](#metadata-history-deltas).
- [x] **Phase 4a+ — sharded content generations.** A content generation is a
  small index of 16,384-entry shards, each with its own manifest and pack, so
  the format has no entry cap and readers load one shard at a time. See
  [Sharded content generations](#sharded-content-generations).

### Outstanding
- [ ] **Phase 3 — wire encoding compaction.** Switch binschema length
  prefixes from fixed `uint32` to varint, drop the always-1 ContentId
  algorithm byte, and shrink low-cardinality arrays to `uint8`/`uint16`
  counts. This is essentially free since the types are generated.
- [ ] **Phase 4b — destructive GC.** Reclaim only objects and content already
  unreachable from every authority in the collection scope. GC must use exact,
  fail-closed marking, include transitive delta-base `ContentId`s, and respect
  writer fencing plus reader grace periods.
- [ ] **Measurement prerequisite.** Before implementing, measure the
  actual benchmark repository: object count by kind, bytes by kind
  (canonical + stored), objects reachable from HEAD vs total ever
  written. The estimates in this doc need grounding.

## Why this exists

A 50-change benchmark wheat repository is roughly 20× larger than
the equivalent git repository. The gap comes from three layers:

1. **No inter-object delta compression.** Every version of every file
   is stored at full size. Git stores the latest version in full and
   older versions as binary deltas (typically 50–200 bytes per delta
   for a one-line change). This accounts for 5–15× on its own.

2. **Richer metadata model with per-node overhead.** Wheat maintains
   7+ object types per change (revision, application, change object,
   view, content manifest, edit, and per-view maps), plus persistent
   radix tries for retained sets, change maps, content sets, and view
   lineage. Each trie node is a separate content-addressed object with
   its own 60-byte segment index entry. With 256-way branching on
   random BLAKE3 digests, inserting one key creates ~2 new nodes
   (O(1)), and unchanged subtrees share across commits via content
   addressing. The cumulative node count is O(N), not O(N²). Still,
   each node's fixed overhead (wire tag, length prefixes, index entry)
   is large relative to its payload, and the per-node storage cost
   (roughly ~70 bytes data + 60 bytes index = ~130 bytes) adds up
   across the ~700–800 trie nodes in a 50-change repository. This
   accounts for ~1.5–2×.

3. **Per-field encoding overhead.** 32-byte BLAKE3 digests (vs git's
   20-byte SHA-1), 4-byte `uint32` length prefixes on every string and
   array, a 1-byte algorithm tag on every ContentId, and 22-byte content
   envelopes vs git's ~4-byte zlib headers. Individually small,
   collectively ~1.3–1.6×.

This document designs the fixes for all three layers.

## Goals

- Bring wheat repository size to within 2× of an equivalent packed git
  repository for typical source-code workloads.
- Preserve content-addressability: every object's identity is still the
  hash of its canonical bytes.
- Keep the read path for HEAD (latest content) fast — O(1) lookups, no
  delta chain traversal for the most recent version of each file.
- Maintain the existing `ContentStore` API (`get`, `put`, `get_bounded`)
  — delta resolution is internal to the store.
- Preserve O(Δ) `validate_transition` — publication validation must
  remain proportional to the change size, not the repository size.

## Non-goals (v1)

- Cross-repository deduplication (monorepo fork sharing). The content
  store is already content-addressed, so identical blobs dedup within a
  repository; cross-repo is a deployment concern.
- Streaming delta application during clone. Materialises the full
  content on read; streaming decompression is an optimisation for later.
- Changing the hash algorithm. BLAKE3-256 is non-negotiable for the
  object identity model. The 12-byte-per-reference overhead relative to
  SHA-1 is a constant factor already paid in every other subsystem.
- Client-directed garbage collection across repositories. A local command may
  operate directly only on an exclusively owned repository store. Shared remote
  structural and content backends require server-authoritative fork-network and
  global mark scopes; Phase 4 specifies that safety boundary rather than a client
  cross-repository sweep.

## Phase 0 — Content delta compression

### How it works

The content envelope already has a codec byte: `0 = raw`, `1 = zstd`.
Add two new codecs:

| Codec | Byte | Payload |
|---|---|---|
| Raw | 0 | `[logical bytes]` |
| Zstd | 1 | `[zstd frame]` |
| Delta | 2 | `[33-byte base ContentId] [delta patch]` |
| DeltaZstd | 3 | `[33-byte base ContentId] [zstd(delta patch)]` |

The base reference is 33 bytes (1-byte algorithm + 32-byte digest)
matching the current `ContentId` wire format. If Phase 3 drops the
algorithm byte, this becomes 32 bytes — but Phase 0 does not depend
on Phase 3.

The **envelope header** (22 bytes: magic + version + codec +
stored_length + logical_length) is unchanged — `stored_length` is
the physical payload size including the base reference, and
`logical_length` is the reconstructed content size.

### Base selection

Wheat has a structural advantage over git here: the Edit object records
the file path and the previous ContentId explicitly. At publish time,
when storing new content, the publisher already knows the old ContentId
for the same path (from the previous tree). That's the natural delta
base — no heuristic search needed.

```
previous_tree[path] → old ContentId (base)
current_tree[path]  → new ContentId (target)
delta = compute_delta(old_content, new_content)
```

The base content is always already in the store — it was published in
a prior commit. There is no write-ordering concern.

If the delta is larger than the full content (happens with binary files
or complete rewrites), store the full content instead. Threshold:
`delta_size + 33 > raw_size` → store raw.

### Delta chain depth

Maximum chain depth: **16**. If storing a delta would create a chain
deeper than 16, store the full content. The `get` path tracks depth
during iterative resolution and returns an error if it exceeds the
limit (corrupt/malicious data).

In practice, a repack pass keeps chains much shorter — see Phase 4.

### Read path

`ContentStore::get_bounded(id, max)` currently returns the raw bytes.
With deltas, resolution is **iterative** to avoid holding all
intermediate buffers on the stack:

```
fn get_bounded(id, max):
    // Collect the delta chain (target → ... → full base)
    chain = []
    current = id
    for depth in 0..MAX_CHAIN_DEPTH:
        record = read_envelope(current)
        if record.codec in [Raw, Zstd]:
            base_bytes = decode(record)
            break
        chain.push(record.patch)
        current = record.base_id
    else:
        return Err(ChainTooDeep)

    // Apply patches in reverse order (base → target)
    result = base_bytes
    for patch in chain.reversed():
        result = apply_delta(result, patch)

    if result.len() > max:
        return Err(TooLarge { minimum: result.len() })
    verify ContentId::of(result) == id
    return result
```

Key detail: the `max` bound applies to the **final reconstructed
output**, not to intermediate base fetches. A base can be larger
than its target (e.g. a file that was truncated). The base fetch
uses `get(base_id)` without a size bound, not `get_bounded`.

### Delta algorithm

Use the **git pack delta format** (copy/insert opcodes) via the
`gix-pack` crate. This is a pure-Rust implementation of the same
delta encoding git uses internally, avoiding a C library dependency.
The format produces compact patches for typical source-code edits.

The delta is then optionally zstd-compressed (codec 3) — the patch
itself often contains literal runs that compress well.

### Storage layout

Content is stored as individual S3 keys via `RemoteContentBackend`
(`{prefix}/content/{first2hex}/{resthex}`), not inside packed segments.
A delta record's base is just another ContentId in the same store —
resolving it is a normal content lookup. There is no segment-boundary
concern for content deltas.

A delta read costs one extra content-store GET per chain link. With
the concurrent `get_many_bounded` path, the base fetch can overlap
with other content reads. Deep chains (up to the depth-16 cap) cost
proportionally more GETs, but repack keeps chains short in practice.

### Expected impact

For a repository where each commit changes one file by a few lines:
- Without deltas: 50 × ~150 bytes average file = ~7.5 KB of content
- With deltas: 1 full copy + 49 × ~60 byte deltas = ~3.1 KB
- Compression ratio on content: **~2.4× for tiny files, 10–50× for
  realistic source files** (where each version is 5–50 KB and the
  delta is 50–500 bytes).

The benchmark's ~130-byte files are a worst case for delta compression
(the delta overhead of 33 bytes + patch header is large relative to the
file). Real source files benefit enormously more. These numbers should
be verified against the stored (post-zstd) sizes, not raw file sizes.

## Phase 1 — Collection compaction

### The problem

Every radix trie node is a separate content-addressed object. A
50-member retained set produces ~50 trie nodes (with 256-way branching,
most keys diverge at the first byte, creating one leaf per key plus
a few internal branch nodes). Each node carries:

- 2-byte wire tag
- 4-byte prefix length + prefix bytes
- 1-byte present/value flag
- 4-byte children count + (1-byte edge + 32-byte child ID) per child
- 60-byte segment index entry

A leaf node with a one-byte prefix and no children is ~12 bytes of data
+ 60 bytes of index entry = ~72 bytes. Across ~700–800 trie nodes in a
50-change repository, that's roughly 50–60 KB of trie overhead.

The trie structure is not gratuitous — `validate_transition` uses
`paired_diff` to compare two trie versions in O(Δ) by skipping
subtrees with identical hashes. **Any compaction strategy must preserve
this property.**

### The fix: higher-fanout nodes with inlined entries

Instead of one leaf node per trie entry, allow nodes to inline multiple
entries up to a size threshold. A node that currently has 5 single-entry
leaf children becomes one node with 5 inline entries. This reduces
object count without losing the Merkle structure:

- Subtrees with identical content still hash identically →
  `paired_diff` still skips them in O(1).
- Large collections still split across nodes at natural boundaries →
  O(log N) lookup is preserved.
- Small collections (< threshold) collapse into a single node →
  minimal overhead.

The threshold should be tuned to keep node objects under ~4–8 KB
(the sweet spot for S3 GET overhead vs per-object index cost).

Concretely, this means changing `collection_builders.rs` to stop
splitting at every byte of the key prefix and instead pack leaves
into their parent node when the combined size stays under the
threshold. The wire format gains an inline-entries array alongside
the existing children array. The `RetainedSetStore` cursor, the
`paired_diff` comparator, and the `bfs_trie_nodes` closure walker
all need to understand the inline entries.

### Expected impact

For a 50-change repository:
- Before: ~700–800 trie node objects
- After: ~100–200 trie node objects (leaves packed into parents)
- Object count reduction: **~70–85%** of trie nodes eliminated
- Size reduction: **~40–50 KB saved** in index entries alone

The structural sharing property is preserved, so `validate_transition`
remains O(Δ). The O(N) cumulative growth is unchanged — it was already
O(N) with the trie, it stays O(N) with higher-fanout nodes.

## Phase 2 — Segment-level block compression

### The problem

Nimble-heron is adding per-record zstd compression to segments (codec
byte 1). This helps with larger objects (file content, large trees), but
small structural objects (trie nodes: ~70 bytes, views: ~162 bytes,
content manifests: ~34 bytes) don't compress well individually — zstd's
overhead for tiny inputs often exceeds the savings.

### The fix: block-level compression

Instead of compressing each record independently or training a custom
dictionary, group consecutive records within a segment into blocks of
~64–128 KB and compress each block as a single zstd frame. Zstd's
sliding window naturally discovers repeated patterns (wire tags, field
layouts, common byte sequences) across records in the same block.

A new codec value encodes this:

| Codec | Byte | Payload |
|---|---|---|
| ZstdBlock | 4 | `[zstd frame containing concatenated records]` |

The segment index entries for records within a block reference offsets
*within the decompressed block*. The block boundary is stored in the
segment directory. On read, decompress the entire block and extract the
record at the given offset.

This avoids dictionary training, dictionary storage, and the question
of how to authenticate a dictionary. It also avoids the problem of a
dictionary that's larger than the records it's trying to compress (which
would happen for very small segments).

### Expected impact

Block-level zstd typically achieves 2–4× on small structured records
that share a schema. For wheat's structural objects:
- Groups of trie nodes compress well (repeated tag + hash patterns)
- Groups of revisions compress well (repeated metadata layouts)
- Trees compress well (repeated name + hash patterns)

Combined with Phase 1 (fewer objects), the structural overhead should
drop to ~1.5–2× relative to git. Exact ratios need measurement.

## Phase 3 — Wire encoding compaction

### Changes to the binschema schema

Update the existing types in `wheat-objects.bs.json5` and re-run
`wheat-gen-schema`. The implementation cost is just editing the schema
— the generated code follows automatically. Note: changing canonical
encoding changes object IDs, so this is a clean-slate change — all
existing objects must be re-encoded.

**1. Varint length prefixes.** Replace `length_type: "uint32"` with
`length_type: "varint"` (LEB128 unsigned) on all arrays and strings.
Most arrays have 0–5 elements: this saves 3 bytes per field (1 byte
varint vs 4 bytes uint32). With 59 `uint32` length-prefix writes in
the generated code, that's ~170 bytes per commit's worth of objects.

Prerequisite: the binschema code generator must support varint length
types. If it doesn't yet, this needs to be added first.

**2. Drop the ContentId algorithm byte.** Currently every ContentId is
`[1-byte algorithm = 1] [32-byte digest]` = 33 bytes. The algorithm is
always 1 (BLAKE3-256) and will remain so for the foreseeable future.
Change ContentId to just `[32-byte digest]` = 32 bytes. If a second
algorithm ever appears, it gets a new type at that point.

This saves 1 byte per file entry in every tree — a tree with 100 files
saves 100 bytes. It also shrinks Phase 0's delta base reference from
33 to 32 bytes.

**3. Compact array counts for low-cardinality fields.** Several arrays
are structurally bounded to small counts:

- `predecessors` (0 or 1 element) → optional flag (1 byte vs 4)
- `dependencies` (typically 0–3) → `uint8` count
- `edits` (typically 1–5) → `uint8` count
- `EditTransition.preserved/added/removed` → `uint8` counts
- `trie children` (0–16 for byte-keyed tries) → `uint8` count

Each saves 1–3 bytes per field, adding up across the thousands of
objects in a repository.

### Quantified savings

Per tree entry:
- Before: 4 (name len) + name + 1 (kind) + 1 (algorithm) + 32 (digest) + 1 (file kind) = 39 + name
- After: 1 (name len varint) + name + 1 (kind) + 32 (digest) + 1 (file kind) = 35 + name
- Saving: **4 bytes per entry** (~10%)

Per trie node (2 children, 4-byte prefix):
- Before: 2 (tag) + 4 (prefix len) + 4 (prefix) + 1 (present) + 4 (children len) + 2×33 = 81 bytes
- After: 2 (tag) + 1 (prefix len) + 4 (prefix) + 1 (present) + 1 (children len) + 2×33 = 75 bytes
- Saving: **6 bytes per node** (~7%)

Per revision (typical single-file change):
- Before: ~500 bytes
- After: ~470 bytes (many 4→1 byte length prefixes)
- Saving: **~30 bytes** (~6%)

Across a 50-change repository with ~1337 objects, the total savings
from Phase 3 alone are roughly **5–10 KB** (~5–8% of structural data).
Modest, but free because the schema regeneration is mechanical.

## Phase 4 — Repack and GC

History retention is not a storage-engine decision. Immutable local revision
checkpoints remain part of the authoritative repository closure until an explicit
`wheat change squash`, `wheat stack squash`, or separately authorized history
rewrite publishes a new state that no longer retains them. The collapse contract,
including stable edit identities and exact publish behavior, lives in
[`wheat-local-cli.md`](./wheat-local-cli.md#revision-checkpoints-and-explicit-collapse).
Neither operation in this section rewrites logical change history.

The two maintenance commands have deliberately different contracts:

- **`wheat repack` is nondestructive and representation-preserving.** It may rewrite
  packed segments, indexes, compression, and physical content-delta envelopes, but
  it preserves the repository head and generation, envelope, canonical structural
  objects, logical `ContentId`s, retained history, and all observable semantics.
  Old physical generations are retired only after the replacement is durable and
  authenticated and reader-safety rules permit retirement.
- **`wheat gc` is destructive but policy-free.** It reclaims only physical objects
  and content already unreachable from every authoritative owner in its collection
  scope. It never decides that a revision, view, conflict, or application is too old
  or uninteresting, never collapses lineage, and never squashes a change. A dry run
  reports candidates; deletion requires an exact fail-closed proof.

A user may therefore run squash without GC, repack without squash, GC without
repack, or any ordered combination. Typical reclamation is semantic collapse first,
then GC after the old objects are outside every root and grace period. Repack is not
a prerequisite for determining semantic deadness, though GC may internally compact
a partially live segment before retiring its old physical representation.

### Delta chain optimization

On repack, rewrite delta chains so the newest version of each file is
the base and older versions are reverse deltas:

```
Before (forward deltas, built during publish):
  v1 (full) ← v2 (delta) ← v3 (delta) ← v4 (delta) ← v5 (delta)
  Reading v5 requires reading v1 + applying 4 deltas

After (reverse deltas, built during repack):
  v5 (full) → v4 (delta) → v3 (delta) → v2 (delta) → v1 (delta)
  Reading v5 is a single lookup
  Reading v1 requires reading v5 + applying 4 deltas
```

This matches real access patterns: HEAD is read far more often than
historical versions.

Note: until repack runs, the fast-HEAD goal does not hold — forward
deltas built during publish require traversing the chain to read the
newest version. Repack should run after a configurable number of
publishes, or as a background task.

### Metadata history deltas

Most of a fresh import's metadata is near-duplicate: each view has its own
ContentSetSnapshot (one full ~180 KiB snapshot per commit on Gothab), and each
commit rewrites the Trees and map nodes on its changed paths. Repack pairs
each object with its successor along the view lineage, using the same rule
as content:

- **Pairing.** For each lineage edge whose child has exactly one parent:
  the parent's and child's ContentSetSnapshots; Trees at the same directory
  path (a paired walk that only descends where IDs differ); RevisionMap and
  ApplicationMap nodes at the same key position.
- **Reverse deltas.** The newer object is the base and the older one is the
  delta. Tips stay whole. An object that two different newer objects claim, or
  that sits on a cycle, stays whole (cycles keep their smallest member whole).
  A delta is kept only if its payload plus the 32-byte base reference is
  smaller than the whole record.
- **Inside segments.** The segment index entry carries an optional
  `delta_base` digest and a `delta_depth`; codecs 2/3 (Delta / DeltaZstd) are
  metadata-only standalone records. A base must be in the same segment, and
  chains are written base-first and contiguously. A chain that would overflow
  a segment is cut and the next record is stored whole. Readers therefore
  resolve and verify a chain within one segment, and the remote store (one
  segment at a time) keeps working.
- **Depth.** At most 16, the same as content (`SEGMENT_MAX_DELTA_DEPTH`).
  Open-time checks: base present in the same segment, depth equals base depth
  plus one, no delta inside a ZstdBlock.
- **Canonical identity is untouched.** Every read re-verifies the
  `ObjectId` of the rebuilt canonical bytes.

The planner and the no-op check are deterministic, so a second repack of an
unchanged store is still a no-op.

### Sharded content generations

A content generation used to be one manifest and one `content.pack`, with a
250,000-entry format cap. It is now:

- `content-generations/{generation}-{digest}.manifest`: the top-level
  **index** (`ContentGenerationManifestV1`). It lists every shard by first
  `ContentId`, entry count, pack length, and the BLAKE3 digest of its
  manifest, plus the content fingerprint used for no-op detection. The catalog
  names the index by its digest; the index's own `manifest_digest` covers the
  generation number, fingerprint, entry count, and every shard reference.
- `content-generations/{generation}/shard-{n:06}.manifest`
  (`ContentGenerationShardV1`) and `shard-{n:06}.pack`. Entries are split in
  `ContentId` order into shards of exactly 16,384 entries; only the last may
  be smaller, and an empty generation has no shards. `pack_offset` is an
  offset in the entry's own shard pack.

A delta base may live in any shard of the same generation. The reader keeps
the small index and opens shards on demand, caching up to 17 (one chain of
16 bases plus its target). Repack writes each shard pack once, in order, then
the shard manifests, then the index last, so an authenticated index implies
its shards were already durable. Cleanup only removes a generation directory
that holds nothing but the shard files its authenticated index names.

The format no longer bounds the entry count. The planner still has a work
bound (`ContentRepackLimits::max_content_ids`, 250,000); see `TODO.md`.

Measured on a disposable Gothab import (208,190 content IDs, 2026-09-24):
13 shards, a 1,091-byte index, ~1.5 MB per shard manifest. Loading the index
takes ~8 µs, loading and verifying one shard manifest ~4 ms, and reading one
envelope from a loaded shard ~1 µs. Store size (205.6 MB) and validate time
(~29 s) are unchanged by sharding.

### Dead object collection

GC uses the complete authority set for the backend's sharing scope:

- an exclusively owned local repository store marks its authenticated current
  envelope plus declared workspace, snapshot, archive, and other pins;
- a shared remote structural store marks the union of every head in its fork
  network;
- a globally deduplicated content store marks the union of content manifests and
  explicit content pins across every sharing domain.

A client must not sweep a shared remote backend from one repository's closure.
Remote collection is server-authoritative. Workspace roots must be durably
registered and enumerable; discovering only the invoking `.wheat/workspace.json`
is not a complete root set.

Collection requires:

1. Snapshot the complete authority set behind an appropriate generation barrier.
2. Completely validate each authority. A best-effort fetch closure that skips
   missing or undecodable references is not a deletion proof.
3. Walk every structural closure with bounded, disk-backed visited state and mark
   all reachable `ObjectId`s and semantic `ContentId`s.
4. **Walk all reachable physical content envelopes and mark delta-base
   `ContentId`s as reachable.** A delta base might not be referenced by a tree or
   semantic manifest but is required to reconstruct reachable content.
5. Repeat delta-base marking transitively to a fixed point.
6. Compare the authenticated mark set with a backend inventory, recheck the
   authority barrier, and retire only the proven complement.
7. Delay physical removal for the configured reader grace period. Whole-dead
   segments may be retired; partially live segments require replacement containing
   the live records before the old segment is retired.

GC fails closed on every missing, corrupt, unreadable, ambiguous, or unbounded
reference. It treats recent loose objects, in-flight publications, staged uploads,
and failed-publication debris according to their declared age/lease/TTL state; it
does not infer deadness merely because one authority snapshot cannot yet reach
them. Writers after a collection barrier belong to the next cycle and must not
publish dependencies on storage being retired by the current cycle.

## Interaction with nimble-heron's codec work

Nimble-heron is currently adding per-record codec support to the packed
segment format (the `codec` byte and `stored_length`/`logical_length`
split in `SegmentIndexEntry`). This is the foundation for all
compression work:

- Phase 0 (content deltas) adds codec values 2/3 to the **content
  envelope**. Content is currently stored as individual S3 keys via
  `RemoteContentBackend`, so delta bases are resolved through the
  same content store — no segment interaction.
- Phase 2 (block compression) adds codec value 4 to the segment
  index, building directly on their work.
- Phase 3 (wire compaction) is orthogonal — it changes the logical
  bytes before any codec is applied.

No conflicts expected. The codec byte is the extension point all
phases share.

## Size budget analysis

Estimated size for a 50-change, 1-file-per-change repository with
~10 KB average file size, compared to git. **These estimates need
verification against real measurements.**

| Layer | Current wheat | After all phases | Git equivalent |
|---|---|---|---|
| Content (file bodies) | 500 KB (50 × 10 KB) | ~60 KB (1 full + 49 deltas + zstd) | ~50 KB |
| Structural metadata | ~120 KB (~1337 objects) | ~50 KB (compacted tries, block zstd) | ~15 KB |
| Index overhead | ~80 KB (~1337 × 60 bytes) | ~30 KB (~500 objects × 60) | ~10 KB |
| **Total** | **~700 KB** | **~140 KB** | **~75 KB** |
| **Ratio to git** | **~9×** | **~1.9×** | **1×** |

The remaining ~1.9× is the cost of wheat's richer metadata model
(more object types per change, 32-byte hashes, more collections to
maintain). This is within the "2× of git" goal, but the estimates
are soft until measured.

## Phasing and dependencies

```
Phase 0 (content deltas)
  └── no dependencies, can start immediately
  └── biggest absolute savings (content is most of the bytes)

Phase 1 (collection compaction)
  └── no dependencies, can start immediately
  └── changes trie builder + collection stores + closure walker
  └── preserves paired_diff and validate_transition

Phase 2 (block compression)
  └── depends on nimble-heron's codec work being stable
  └── builds on per-record codec in segments

Phase 3 (wire compaction)
  └── may depend on binschema varint support
  └── changes canonical encoding → changes all object IDs
  └── smallest absolute savings, but essentially free

Phase 4a (nondestructive repack)
  └── depends on Phase 0 for content delta-chain reversal
  └── preserves semantic authority and logical identities
  └── needs atomic physical-generation publication and reader-safe retirement

Phase 4b (destructive GC)
  └── independent from semantic squash and optional repack scheduling
  └── needs complete owner enumeration, exact fail-closed marking, inventory,
      writer fencing, delta-base traversal, and reader grace periods
  └── remote mode is server-authoritative at fork-network/global scope
```

Phases 0 and 1 are independent and deliver the largest wins. Phase 2
is a nice-to-have after the codec plumbing stabilises. Phase 3 can
be done at any point but touches everything. Phase 4 contains two
separate ongoing maintenance operations: representation-preserving repack and
destructive reachability GC.

## References

- `crates/wheat-core/schemas/wheat-objects.bs.json5` — current wire schema
- `crates/wheat-store/src/codec.rs` — content envelope format
- `crates/wheat-store/src/packed/segment.rs` — segment format
- `crates/wheat-core/src/collection_builders.rs` — radix trie builders
- `crates/wheat-core/src/collection.rs` — trie node types
- `crates/wheat-store/src/closure.rs` — structural closure computation
- `crates/wheat-store/src/repository_transition.rs` — O(Δ) validation
- `crates/wheat-store/src/radix.rs` — `paired_diff` implementation
- `gix-pack` crate — pure-Rust git pack delta format
