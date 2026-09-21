# Wheat Storage Efficiency — Design

Status: partial — Phases 0–1.5 landed, Phases 2–4 outstanding
Owner: bart
Last updated: 2026-09-21

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

### Outstanding
- [ ] **Phase 2 — segment-level block compression.** Compress groups
  of consecutive records within a segment as shared zstd frames,
  so small structural objects benefit from cross-record context.
- [ ] **Phase 3 — wire encoding compaction.** Switch binschema length
  prefixes from fixed `uint32` to varint, drop the always-1 ContentId
  algorithm byte, and shrink low-cardinality arrays to `uint8`/`uint16`
  counts. This is essentially free since the types are generated.
- [ ] **Phase 4 — repack / gc.** Background compaction that rebuilds
  delta chains (newest-as-base) and reclaims unreachable objects.
  GC must mark delta base ContentIds as reachable.
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
- Garbage collection across repositories. Repack/GC operates on a
  single repository's object closure.

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

## Phase 4 — Repack / GC

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

### Dead object collection

After repacking, objects that are no longer in any reachable closure
can be garbage collected. This requires:

1. Walk the current envelope's full closure
   (`collect_structural_closure`).
2. Mark all reachable ObjectIds and ContentIds.
3. **Walk all reachable content envelopes and mark delta base
   ContentIds as reachable.** This is critical: a delta base
   might not be referenced by any tree or manifest in the current
   closure, but it's required to reconstruct a content blob that
   *is* reachable. Without this step, GC would delete delta bases
   and make dependent content unrecoverable.
4. Repeat step 3 transitively until no new bases are discovered
   (a base might itself be a delta referencing another base).
5. Delete everything else from the segment and content stores.

GC must fail closed on missing or unreadable references — if an
object can't be read, assume it's reachable rather than deleting
something that might depend on it.

For the remote packed store, this means building a new segment from
only the live objects and uploading it, then deleting the old segments.

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

Phase 4 (repack / gc)
  └── depends on Phase 0 (delta chains to rewrite)
  └── GC must understand delta base references in content envelopes
  └── can run incrementally against the current format
```

Phases 0 and 1 are independent and deliver the largest wins. Phase 2
is a nice-to-have after the codec plumbing stabilises. Phase 3 can
be done at any point but touches everything. Phase 4 is the ongoing
maintenance tool.

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
