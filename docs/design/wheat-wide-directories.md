# Wheat wide directories — Design

Status: partial (2026-10-26): option A, threshold 256, buckets of 2; steps 0 to 5
and the switch-on landed, so every writer stores a directory wider than 256
entries chunked. Step 6 (benchmarks and contracts, and the memoization question)
and the docs remain. A tree-format change, so it lands before the V1 format
freeze.
Owner: bart
Last updated: 2026-10-26 (steps 3, 5 and the switch-on)

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

- [x] **Decision — chunk structure (2026-10-26).** Option A, the radix over
  `BLAKE3(name)`. Bart asked Codex and Antigravity and took the majority: both
  voted A. See [Decision](#decision).
- [x] **Decision — the flat-directory threshold (2026-10-26).** `T` = 256.
- [x] **Decision — component name length (2026-10-26).** At most 255 bytes.
- [x] **Step 0 — component name limit.** `MAX_DIRECTORY_COMPONENT_BYTES` (255)
  in the shared `validate_directory_component`, so capture, Git import (a
  wider Git name is refused with `InvalidDirectoryComponent`) and canonical
  validation all refuse longer names, and a chunk node's bytes are bounded.
- [x] **Decision — where the count lives (2026-10-26).** A `TreeId` names a
  small `ChunkedTreeV1` header `{count, root}`; the chunk nodes are a plain radix
  collection. Bart did not answer in time, so this was the default; see
  [Format](#format).
- [x] **Decision — leaf buckets (2026-10-26).** `B` = 2: up to two entries sharing
  their next key byte stay inline. Bart chose it from the measurements in
  [Format](#format).
- [x] **Step 1 — format.** `ChunkedTreeV1` (0x1501) and `DirectoryChunkNodeV1`
  (0x1601), object kinds 23 and 24, `DirectoryChunkId`, core types, canonical
  validation of both (count above 256, bucket rule, every entry's suffix the tail
  of its name's key), the reference builder `build_directory`, and the
  references `object_graph` follows.
- [x] **Step 1b — radix buckets in the store.** `radix.rs` takes a
  per-collection `Collection::BUCKET` (1 for every existing collection, 2 for the
  new `DirectoryChunkCollection`) in `validate_node_at`, updates and diffs, and
  `Collection::Value` relaxes from `Copy` to `Clone`. A reader below the root also
  checks the rule's non-local half: a node without children holds more than `B`
  entries. A diff pairs a bucket with a subtree on the same byte as the node the
  bucket would be, which is why `B` is at most 2. Tests in `directory_chunks.rs`
  check that incremental updates land on the IDs `build_directory` makes and that
  diffs match the entry sets.
- [x] **Extended beyond the plan — key-ordered radix diffs.** `paired_diff`
  promised key order but emitted a node's inline entry diffs before its child
  subtrees, for every collection. Entry diffs now queue between the subtrees they
  sort between.
- [x] **Step 2 — store reads.** `merkle_tree/directory.rs` opens a `TreeId` as
  either layout:
  - `directory_get` looks a name up along its key path, caching the chunks it
    read per open directory, so `path_shapes` reads each chunk at most once;
  - `directory_entries` lists a directory in name order; a chunked one is read
    whole, checking each entry's key path and the header's count;
  - `directory_diff` compares two chunked directories through the radix diff,
    reading only the chunks where they differ.

  `lookup`, `list_directory`, `path_shapes`, `walk_path`, `select_paths`,
  `walk_files`, `content_cursor`, `TreeDiffCursor`, `read_node` (and so
  hydration, full verification, Git export's reads) and `verify_untouched` use
  it, sharing one node, entry and byte accounting (`ReadLimits`/`ReadCount`)
  instead of five copies. Tests in `wheat-store/tests/wide_directories.rs`
  count the reads of a lookup, a sorted batch and a diff. The object-graph walks
  that decode `TreeV1` themselves are step 4.
- [x] **Step 4 — walks over the object graph.** A wide directory's chunks are
  `RadixKind::Directory` in `object_graph`, so:
  - `StructuralDelta` (fetch, push, publish) walks a chunked tree's radix like
    any other and pairs each subdirectory with its old counterpart by name key,
    whichever layout either side has (both chunked through the radix; a flat side
    as the key list it holds; a directory that shrank to flat by looking each
    subdirectory up in the old radix);
  - metadata repack's `pair_trees` pairs two chunked directories' chunks at the
    same edge path and their changed entries by name, walking a chunk with no
    counterpart so an entry that moved between a bucket and a child chunk still
    pairs; `reachable_from_view` keeps every chunk a tip reaches whole;
  - the client's `store analyze` content classifier reads chunks;
  - the snapshot closure walk already followed `for_each_reference`, which lists
    a header's root and a chunk's children and entries;
  - `staged_metrics` sizes its per-kind counters from `ObjectKind::HIGHEST_CODE`
    (it panicked on kinds above 18, already true of 21 and 22).

  Tests: `structural_delta` (only what changed comes back, across the threshold
  both ways, nothing paired is read), `metadata_repack` (entry pairs and
  same-kind proposals as buckets turn into chunks, tips keep every chunk), and the
  CLI's `store_analyze` wide-directory case. Full verification and staged
  import read a chunked tree whole through `read_node`, which is correct; see
  per-chunk memoization below.
- [x] **Step 3 — store writes.** `merkle_tree/directory.rs` writes both layouts:
  - `write_directory` applies a batch of a directory's changes to the directory
    it opened. Staying chunked, it updates the radix along the touched chunks
    only, serving the chunks the open directory already read from memory; a batch
    that crosses `T` either way rebuilds the directory in the other layout from
    its entries. The new count comes from each name's slot (empty, held, or a
    different name with the same key, which is refused).
  - `write_entries` builds a directory from scratch through `build_directory`
    (`write_node`, empty directories).

  `apply_edits` and `final_update` both go through it. `final_update` stages its
  writes in a `BatchObjectStore` and checks its byte budget against the staged
  size, so `FinalTreeUpdateLimits` lost `max_node_bytes`: the format now bounds
  a node (256 entries of names up to 255 bytes) and a chunk. `select_paths` looks
  selector names up in a directory instead of listing it, so a selected path in
  a wide directory reads its key path, not every chunk. Tests in
  `wide_directories.rs`: batches crossing the threshold both ways, alternating
  both writers, land on `build_directory`'s IDs, and one change reads and writes
  only the chunks on its path. The I/O tests that assumed one node per directory
  (`merkle_tree.rs` lookups and selections, `final_tree_update.rs`
  `representative_wide_update_has_linear_io_budget`) count chunks now.
- [x] **Switch-on.** `wire::tree_objects` builds with `build_directory`, so core
  (`Repository`, the client's capture) writes the same IDs as the store, and
  canonical validation refuses a `TreeV1` with more than 256 entries. The server
  fetch test that needed a structural object larger than a batch used a
  250,000-entry directory; it uses a change with a long description now.
- [x] **Step 5 — Git bridge.** No code of its own: import writes trees through
  the store's `apply_final_edits`, so a wide Git tree is stored chunked, and
  export lists each directory with `read_node`, in name order, into one Git tree.
  `wheat-cli/tests/git_wide_directory.rs` pulls a 300-file Git directory, changes
  one file and pushes (Git sees only that file change, `fsck --strict` passes),
  then pulls a Git change to another file.

### Outstanding

- [ ] **Per-chunk memoization in verification and import (measure first, in
  step 6).** `repository_verify`'s `authenticate_tree` and `tree`, and staged
  import's `authenticate_tree_once`, memoize per tree, so each new version of a
  wide directory reads all its chunks: the bytes a flat `TreeV1` costs today, in
  more objects. Skipping chunks proven before needs each skipped chunk's entry
  count (for the header's count check), its subdirectories' greatest height and
  its per-aggregate content check carried in the memo, in three walks. Decide
  from the step 6 numbers.
- [ ] **Step 6 — benchmarks and contracts.** A contract on the flat layout in
  `increment_validate` (reads and bytes per create and per validation grow with
  the logarithm of the directory).
- [ ] **Docs and TODO.md.** Close "A change in a large directory reads and
  writes the whole directory" and update `server-native-source-control-store.md`
  ("deterministic shards for exceptionally wide directories").

## Why this exists

A directory is one `TreeV1` record. Changing one entry re-encodes, hashes and
writes the whole record, and every reader that touches the directory decodes all
of it. `increment_validate`'s flat history (all files in one directory, release,
2026-10-26):

| Directory size | Create (mean) | Validate one change: reads | bytes | wall |
|---|---|---|---|---|
| 1,000 | 13 ms | 116 | 0.63 MB | 3.3 ms |
| 10,000 | 22 ms | 125 | 4.65 MB | 16.6 ms |

Building the 10,000-file history takes 116 s because each create rewrites the
directory: the cost of a history in one directory is the square of its size.
Directories spread over levels, as in source trees, stay cheap (0.54 MB per
validation at 100,000 changes), but generated directories, vendored packages,
fixtures and asset folders reach tens of thousands of entries.

Every tree ID depends on the layout, so this has to land before the format
freeze. Afterwards it would be a rewrite of every tree in every repository.

## Goals

- A one-entry change to a directory of `n` entries reads and writes `O(log n)`
  nodes of bounded size, not the directory.
- Name lookups, sorted multi-lookups and two-directory diffs cost `O(log n)` per
  name or per difference. Iterating a whole wide directory in name order costs a
  sort of its entries (see [Decision](#decision)).
- **Canonical:** the same entries give the same tree ID whatever edits produced
  them, so a tree's ID stays a function of its content (history independence).
- Directories of at most `T` entries, nearly all of them, keep today's single
  `TreeV1` record, so ordinary trees and their costs do not change.
- Node sizes stay bounded against adversarial names: a crafted directory must
  not produce an unbounded node or chain.

## Non-goals (v1)

- Chunking file content (blobs already have their own chunking in the protocol).
- Changing path rules (component validation, depth caps) or the flat `Tree` model
  in core.
- Sparse or lazy fetch of directory chunks (deferred with lazy structure in
  `wheat-lessons-from-scion.md`).

## The canonical rule

A directory with at most `T` entries is one `TreeV1`. A directory with more is a
chunked directory: a `DirectoryEntryRefV1` keeps naming a `TreeId`, and the
object behind it is the chunk structure's root under a new wire tag. Validation
refuses a `TreeV1` with more than `T` entries and a chunked root with `T` or
fewer, so each entry set has exactly one encoding. Readers dispatch on the wire
tag, so a `TreeId` still means "a directory" everywhere it appears (views,
applications, parent directories).

## Options

The three structures share the rule above and differ in how a wide directory's
entries are split.

### A. Radix over a hash of the name

Key each entry by `BLAKE3(name)` (32 bytes) in the existing 256-way radix
(`radix.rs`), with the value carrying the name and the entry.

- **Reuse:** `radix.rs` already handles fixed 32-byte keys, inline entries,
  batched updates, paired diff, cursors, and both builders. Only a new
  `Collection` with a richer value type is needed.
- **Shape:** keys are uniformly spread whatever the names look like, so a
  directory of `n` entries is about `log_256 n` levels deep: 2 reads for
  10,000 entries, 3 for a million. Crafting names that collide on hash
  prefixes costs a hash search per byte of depth (birthday bound), so depth
  stays bounded in practice.
- **Order:** chunks are in hash order, not name order. Whole-directory callers
  that need name order (`walk_files`, materialization, `select_paths`, Git
  export, diff output) sort each wide directory's entries in memory. That costs
  `O(n log n)` time and `O(n)` memory per directory visited. They already hold
  `O(n)` data for it, but not all at once today. A two-directory diff pairs by
  hash and sorts only the differences.
- **Size:** each inline entry carries the rest of its 32-byte hash beside its
  name, about 30 extra bytes per entry in wide directories.

### B. Radix over the name bytes

Key each entry by its name followed by `0x00`. Names cannot contain NUL, so the
keys are prefix-free and keep name order.

- **Order:** name order, so iteration and diffs stream as today.
- **Reuse:** partial. `radix.rs` assumes fixed-length keys throughout:
  - `Key::LEN`;
  - the suffix-length checks in `validate_node_at`;
  - the fixed key buffers and `KeyPath`/`RadixPath` arrays in
    `structural_delta.rs` and `snapshot.rs`;
  - the core builder's equal-length assertion.

  Generalizing it to variable-length keys touches every collection.
- **Shape:** depth follows the names' structure, not their number. `f1.txt` to
  `f99999.txt` branch at every digit position, so a lookup reads one node per
  branching byte (about 5–6). Many names sharing a long prefix compress into one
  node's `prefix`. Crafted names can force one level per byte up to the name
  length, bounded only by a name-length limit, which Wheat does not have today.
- **Size:** many small nodes: roughly one per branching point, each with its
  own 32-byte ID and store index entry.

### C. Content-defined B-tree (prolly tree)

Split the sorted entries into leaves at names whose `BLAKE3(name)` has its low
`k` bits zero (an expected `2^k` entries per leaf, say 64), with a cap that
forces a split at `L` entries. Build parent levels the same way over the leaves'
first names.

- **Order:** name order, so iteration and diffs stream.
- **Shape:** balanced, `log_64 n` levels: 2 reads for 10,000 entries, 3 for
  250,000. Node sizes cluster around a few kilobytes, whatever the names.
- **Canonical:** boundaries depend only on the names, so the same set always
  splits the same way.
- **Edits:** an insert or delete changes its leaf and, when it adds or removes a
  boundary, merges or splits it with a neighbour; parents change along one path.
  Names crafted to have no boundary hit the cap, and an insert in such a run can
  shift every forced boundary after it up to the next natural one. That is at
  most the run's length, which the author of those names pays for.
- **Reuse:** none. It needs a new builder in core and in the store, a cursor, an
  update with boundary resync, and a range-aligned diff.

### Costs at a glance

| | A. hashed radix | B. name radix | C. prolly tree |
|---|---|---|---|
| Reads per lookup, 10k entries | 2 | ~5 | 2 |
| Iteration in name order | sort per directory | streams | streams |
| Diff of two versions | by hash, sort the Δ | streams | streams, range-aligned |
| Adversarial depth | hash-search bounded | name-length bounded | balanced (cap `L`) |
| New machinery | a `Collection` | variable-length keys in `radix.rs` | a new structure |
| Extra bytes per entry | ~30 (hash suffix) | small | small |

## Decision

Option A, with `T` = 256 (Bart, 2026-10-26, by majority of a Codex and an
Antigravity review; both voted A). What the reviews added, which the plan
follows:

- **Values are not `Copy`.** `radix.rs`'s `Collection::Value` and the core
  builder require `Copy`; a chunk entry owns its name. Relax the bound to
  `Clone` and keep clones off the hot paths (entries move into nodes, readers
  borrow).
- **Each entry's key is checked.** Validation recomputes `BLAKE3(name)` for every
  entry of a chunk node and requires it to equal the key path the entry sits at;
  otherwise one name could be filed under another's key.
- **Full-hash collisions refuse.** Two names with the same BLAKE3 hash cannot
  both be stored; an insert that would replace a different name is an error,
  never an overwrite.
- **Name order costs a sort.** Ordered iteration of a wide directory is
  `O(n log n)` time and `O(n)` memory, not linear, so the goal below is amended.
  Today's readers already decode a whole directory into a `BTreeMap` and Git
  export already sorts, so this is no regression. Diffs prune equal subtrees by
  ID and sort only the differences (`O(Δ log Δ)`); sorted multi-lookups group
  their names by hash.
- **The threshold is counted, not scanned.** Each chunked root records its entry
  count, so crossing 256 is known without reading the directory, and the count
  is validated.
- **Node bytes need a name bound.** A fixed key bounds depth and fan-out, but a
  node of 256 inline entries is only bounded if names are. Components are at
  most 255 bytes (Bart, 2026-10-26).
- **Lookup figures are typical, not guaranteed.** The benchmark covers bytes,
  node counts, whole walks and the 256↔257 transition, not only reads per
  lookup.

## Format

A `TreeId` names one of two objects:

- `TreeV1`, a flat directory of at most 256 entries;
- `ChunkedTreeV1 {count, root}`, a directory of more, whose entries are in a radix
  of `DirectoryChunkNodeV1` nodes. `count` is how many entries the directory holds.
  A writer uses it to know when a removal crosses back to 256, which turns the
  directory flat again, without reading every node.

The header costs one extra read of 36 bytes per wide directory a reader visits.
Putting the count in the root node instead would save that read, but `radix.rs`
would then have to carry a field that only the root has through every rewrite.

A chunk node is a radix node over the 32-byte key `BLAKE3(name)`, with prefix,
children and inline entries; an entry is `{suffix, name, value}`, where the suffix
is the rest of the name's key. Unlike other collections it keeps a *bucket* of up
to `B` = 2 entries that share their next key byte inline, and only a larger group
becomes a child.

**Why buckets.** With single-entry buckets, as in every other collection, each
collision on a key byte becomes its own two-entry node. 257 entries spread over
256 first bytes leave about 70 collisions, so the directory takes 72 objects.
Measured with the core builder (names like `file-00042.txt`, 14 bytes):

| Entries | Flat | `B`=1 | `B`=2 | `B`=4 | `B`=8 | `B`=16 |
|---|---|---|---|---|---|---|
| **Mean bytes a one-entry change rewrites** | | | | | | |
| 257 | 12.6 KB | 10.5 KB | 17.5 KB | 21 KB | 21 KB | 21 KB |
| 1,000 | 49 KB | 9.4 KB | 13.9 KB | 39.5 KB | 80 KB | 82 KB |
| 4,000 | 195 KB | 9.7 KB | 9.8 KB | 10.1 KB | 13.6 KB | 170 KB |
| 10,000 | 489 KB | 11.3 KB | 11.7 KB | 11.7 KB | 11.7 KB | 11.7 KB |
| 100,000 | 4.9 MB | 19.4 KB | 27.7 KB | 38 KB | 40 KB | 40 KB |
| **Objects** | | | | | | |
| 257 | 1 | 72 | 18 | 1 | 1 | 1 |
| 1,000 | 1 | 240 | 199 | 96 | 3 | 1 |
| 10,000 | 1 | 948 | 289 | 257 | 257 | 257 |
| 100,000 | 1 | 30,081 | 13,219 | 1,534 | 258 | 257 |

A larger `B` leaves fewer objects. The cost is that the root can hold up to
`256 × B` entries inline, so a directory below that size is one node, and a
change rewrites all of it.

**Size.** Total bytes are 1.7–1.9× flat for every `B`. Most of the overhead is the
stored suffix, about 32 bytes per entry against names of 14 bytes here. Dropping
the suffix would bring the total to about 1.1× flat, because readers would
recompute `BLAKE3(name)`. Bart kept the suffix.

**Validation.** Checked within one node:

- The suffixes are sorted and all the same length.
- Each suffix is the tail of its own name's key.
- No bucket exceeds `B` or shares its byte with a child.
- The node branches on at least two bytes.

Checked by a reader walking from the root:

- The key bytes above a node.
- A node below the root with no children holds more than `B` entries.
- `count` matches the entries, checked by a full walk.

## Callers

From the survey of the tree today, grouped by what they need:

- **Name lookup / sorted multi-lookup:** `merkle_tree.rs` `lookup`,
  `path_shapes`/`lookup_many` and their callers:
  - `repository_mutation.rs`, `provenance.rs`, `commit_writer.rs` and
    `semantic_checks.rs::replay`;
  - in the client, `workspace.rs` and `capture_settlement.rs`;
  - in the Git bridge, `staged_import.rs`;
  - `wheat-cli` `main.rs`.
- **Ordered iteration:**
  - `walk_files`, `walk_path`, `select_paths`, `content_cursor`;
  - `workspace_scan.rs`, `ignore_import.rs`, materialization's `target_files`;
  - full verification (`repository_verify.rs`), hydration, I7's
    `content_map_matches_tree`, `store_analyze`;
  - Git export's `write_tree`.
- **Two-directory diff:**
  - `TreeDiffCursor` (materialization's `plan_delta`, `repository_diff.rs`, the
    server's content delta bases);
  - `semantic_checks.rs::verify_untouched`, `content_map_follows_tree_diff`;
  - `StructuralDelta::run_tree`, metadata repack's `pair_trees`.
- **Writers:** `edit_node`/`apply_edits`, `final_update::apply` (its staging
  budget charges whole nodes), `wire::tree_objects`.
- **Object-graph walks:** `object_graph::tree`, the snapshot closure walk,
  `staged_import.rs::authenticate_tree_once`.

## Phasing

The steps in the checklist, in order. Steps 2 and 3 can land with only a flat
layout behind the new reader and writer (no behaviour change), then step 1's
chunked layout switches on.

Step 1 has landed with the format, validation and core builder, but nothing
writes chunks yet. Readers can be built and tested against `build_directory`'s
output before any writer switches over.

## Open questions for review

_(none open)_

## References

- `TODO.md`, "A change in a large directory reads and writes the whole directory
  (2026-10-22)".
- `docs/design/wheat-incremental-validation.md`, the flat measurements.
- `docs/design/server-native-source-control-store.md`, trees and wide-directory
  sharding.
- `docs/design/wheat-storage-efficiency.md`, radix node shape and repack pairing.
