# Wheat large content — Design

Status: proposed; Q1–Q4 decided 2026-11-01, Q5 and the re-chunking half of Q2 open
Owner: bart
Last updated: 2026-11-01

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land and move them from Outstanding to Done.

### Done

- [x] **Decision — scope (2026-11-01).** Bart: "Everything". Receiving, sending,
  storing, repacking and every client path stream large blobs; none holds one
  whole.
- [x] **Decision — what a new version of a large blob costs (2026-11-01).** Bart:
  "for very large blobs what we do instead is store only changed chunks".
  Content-defined chunks, not deltas, for blobs over the threshold.

- [x] **Decision — the chunking threshold (Q1, 2026-11-01).** Over 16 MiB, today's
  `wheat add` limit. Blobs up to 16 MiB stay whole envelopes and keep deltas.
- [x] **Decision — chunk sizes (Q2, 2026-11-01).** FastCDC with a 1 MiB minimum,
  2 MiB average and 4 MiB maximum: about 500 chunks for 1 GiB.
- [x] **Decision — chunk identity (Q3, 2026-11-01).** A chunk is content: its ID
  is the `ContentId` of its bytes.
- [x] **Decision — the size limit for one file (Q4, 2026-11-01).** Bart: Gothab
  allows 1 TB, "not because I ever expect anyone to store that, but because the
  format has to support it". The format takes blobs up to 1 TiB; see
  [Size](#size). Which limits the tools apply below that is part of step 7.

### Outstanding

- [ ] **Decision — the chunker (Q5).** See [Q5](#open-questions-for-review).
- [ ] **Decision — re-chunking a manifest cut elsewhere (Q2b).** See
  [Q2b](#open-questions-for-review).
- [ ] **Step 1 — format.** The chunker, the `Chunked` envelope codec and its
  manifest record (BinSchema), canonical validation of a manifest.
- [ ] **Step 2 — store.** `ContentStore::writer` and `ContentStore::reader`
  ([Store API](#store-api)) over every backend: loose files, the packed store,
  object-storage blobs. `get`/`get_bounded` keep working for blobs that fit their
  bound.
- [ ] **Step 3 — protocol.** Manifest-first transfer of chunked blobs, chunk-level
  have/want, receivers that hash as chunks arrive and stream into the store,
  senders that read chunk by chunk ([Protocol](#protocol)).
- [ ] **Step 4 — client.** Capture (`wheat add`), materialization, diff, `cat`,
  edit move and integrate read and write large blobs through the streaming API.
- [ ] **Step 5 — Git bridge.** Import and export stream large blobs.
- [ ] **Step 6 — repack, verification, bundles.** Content repack and generations
  hold chunks and manifests; full verification and network export/import stream.
- [ ] **Step 7 — benchmarks and contracts.** Cases for a large file's add,
  publish, fetch, a small edit to it, and materialization, with peak memory and
  bytes written/sent as budgets.

## Why this exists

Every content path holds a blob whole as a `Vec<u8>`, up to 1 GiB
(`HARD_LOGICAL_RECORD_BYTES`, `MAX_CONTENT_SIZE`). The protocol already sends a blob
over 4 MiB as `ContentChunk`s, but both ends hold it whole: the receiver
reassembles the chunks into one buffer (server publish, client fetch), and the
sender reads the blob whole before cutting it up (server fetch, client publish).
Storing it then makes the envelope as another whole buffer, and a delta against an
older version needs both versions and the patch in memory at once. A server taking
a few large uploads at a time needs gigabytes per upload, and a 1 GiB file changed
by a few bytes costs a full new compressed copy wherever a delta is not made.

Gothab takes large uploads by spooling them to a temporary file, hashing as the
bytes arrive, and storing from the file; above 100 MiB its object storage writes go
as multipart uploads. `server-native-source-control-store.md` already expected the
same for Wheat: "Large files remain ordinary tree entries addressed by one
whole-file `ContentId` … The content backend may physically retain that identity as
a whole object or as a manifest of content-defined chunks … Native streaming
put/get and bounded FastCDC-style chunk storage remain to be designed." This is
that design.

## Goals

- Memory per transfer, write and read of a blob is bounded by the threshold
  (16 MiB) for a whole blob and by a chunk (at most 4 MiB) for a chunked one, not
  by the blob.
- A new version of a large blob stores and sends only the chunks it does not share
  with versions the other side already has.
- Trees, edits, revisions and every other identity stay as they are: a file is
  still one `ContentId`, the hash of its bytes. Chunking is how content is stored
  and sent, not what it is.
- One way to store a large blob on every backend. Chunks fit under the protocol's
  message size and under object storage's single-request size, so no multipart
  uploads are needed.

## Non-goals (v1)

- Chunking small blobs. Below the threshold blobs stay whole envelopes, with deltas
  as today.
- Deltas between chunks. A changed chunk is stored whole (compressed).
- Resuming an interrupted transfer partway through a blob. Chunks already stored
  are not sent again (have/want), which covers most of it.
- Content GC. There is none yet; when it comes, it follows manifests to chunks.

## Format

A blob over the threshold is cut into content-defined chunks with FastCDC
(Gear-hash rolling boundaries, normalized chunking). A boundary depends only on the
bytes around it, so an edit moves the boundaries near it and leaves the rest, and
two versions share every chunk the edit did not touch.

The blob is stored under its `ContentId` as an envelope with a new codec,
`Chunked`, whose payload is a manifest:

```
ContentChunkManifestV1 {
  logical_length: uint64          // the blob's length; the chunks' lengths sum to it
  chunks: array<ContentChunkRefV1> // in order
}
ContentChunkRefV1 {
  id: Digest256                   // the chunk's identity (Q3)
  length: uint32                  // at most the maximum chunk size
}
```

Each chunk is stored as an ordinary whole envelope (raw or zstd) under its own ID.
A manifest for a 1 GiB blob with 1 MiB average chunks has about 1,000 entries
(36 KiB). Canonical validation of a manifest checks the lengths sum to
`logical_length`, every chunk is within the sizes the chunker produces, and the
blob is over the threshold. It cannot check from the manifest alone that the
chunks are where FastCDC would cut, and need not: the blob's identity is its
bytes, so a manifest cut differently stores the same content, only with less
sharing. A receiver can re-chunk what it is sent if that matters (Q2).

The chunker's parameters are part of the format: changing them later changes where
new versions cut, and so how much they share with old ones, though never what any
`ContentId` names.

### Size

The format takes blobs up to 1 TiB (Q4). A 1 MiB minimum chunk bounds a 1 TiB
blob at 1,048,576 chunks, so its manifest is at most about 36 MiB: within one
record (`HARD_LOGICAL_RECORD_BYTES` is 1 GiB, a segment payload 128 MiB), read
whole when a reader opens the blob. At the 2 MiB average a 1 TiB blob's manifest
is about 18 MiB, a 1 GiB blob's about 18 KiB. `logical_length` is a `uint64`
and a chunk's length a `uint32`, so neither field limits the size.

## Store API

`ContentStore` gains streaming halves next to `put`/`get`:

- `writer() -> ContentWriter`: an `io::Write` that hashes the bytes for their
  `ContentId` as they come. Up to the threshold it buffers them; past it, it cuts
  chunks as it goes and stores each one at once, so it holds at most one chunk.
  `finish(expected: Option<ContentId>)` checks the ID, stores the manifest (or the
  whole envelope for a small blob) and returns it. A writer dropped unfinished
  leaves only chunks, which are content-addressed and harmless.
- `reader(id) -> ContentReader`: an `io::Read` over the blob's bytes: a whole
  envelope decoded (small, so in memory as today), a chunked one read chunk by
  chunk. Each chunk's bytes are checked against its ID as they are read; the
  whole blob's `ContentId` is checked when the reader reaches the end, so a reader
  that stops early has checked what it returned chunk by chunk.
- `get`/`get_bounded` keep working, assembling a chunked blob that fits the bound.

Backends store chunks and manifests as they store envelopes today: loose files or
segment records in the packed store (a chunk of 1 MiB is over the 128 KiB loose
threshold, so loose until repacked), one object per chunk in object storage. The
crate-private `ContentBackend` needs nothing new for chunks; it needs a way to
store a manifest envelope whose codec it does not decode.

`put_many_with_bases` skips deltas for chunked blobs: a base is only a hint.

## Protocol

Today a blob over 4 MiB goes as `ContentChunk`s carrying offsets. With chunked
storage, a transfer of a large blob becomes:

1. The sender sends `ContentManifest { op_id, id, manifest }` in place of the blob.
2. The receiver looks up which chunk IDs it has (`existing_ids`), and asks for the
   rest with `ContentNeeded` (publish) or `ContentFetchRequest` (fetch), as it asks
   for blobs today.
3. The sender sends the chunks as ordinary content in `ContentBatch`es (a chunk is
   at most 4 MiB, so it fits the batch's per-item limit); the receiver checks each
   against its ID and stores it.
4. With every chunk present, the receiver reads them in manifest order through a
   `ContentId` hasher, checks the blob's ID and stores the manifest. This reads the
   chunks it already had as well: O(blob) disk reads, a chunk of memory.

Senders read through `ContentStore::reader` a chunk at a time. Blobs at or under
the threshold travel as they do now: whole in a `ContentBatch` up to 4 MiB, as
offset-based `ContentChunk`s from 4 to 16 MiB, held whole by both ends (at most
16 MiB). The receiver's per-item limit for `ContentChunk` drops from 1 GiB to the
threshold.

## Callers

What changes outside the store (from the inventory of whole-blob paths,
2026-11-01):

- **Client capture.** `wheat add` reads a file whole up to 16 MiB
  (`DEFAULT_MAX_INPUT_BYTES`). Large files are written through
  `ContentStore::writer` from the file. Text capture (newline canonicalization)
  applies to text files, which stay under the threshold, or streams through the
  writer.
- **Materialization** writes files from `ContentStore::reader` with a fixed
  buffer; its 256 MiB per-file limit goes with Q4.
- **Diff, `cat`, browse** already read bounded; they read through the reader up
  to their bound.
- **Edit application and integrate** resolve whole blobs for line edits. A large
  file's change is a whole-file replacement (as the server-native design says), so
  they take its `ContentId` without reading it.
- **Git import** captures a gix blob whole; a blob over the threshold streams from
  gix's object reader into the writer. **Git export** writes a large blob to Git
  through a stream (Git's blob hash needs the length first, which the manifest
  has).
- **Repack** treats chunks as ordinary content (delta-free) and manifests as small
  records; its 64 MiB per-record limit no longer meets large blobs.
- **Verification and bundles** (`repository_verify`, network export/import) read
  through the reader and copy chunk by chunk.

## Phasing

1. Decide Q1–Q4.
2. Format: chunker (with its parameters fixed in tests: known inputs cut at known
   offsets), codec and manifest record.
3. Store API over every backend, with tests counting peak buffered bytes.
4. Protocol: manifest-first transfer, both directions.
5. Client, Git bridge, repack, verification, bundles, in that order.
6. Raise or replace the size limits (Q4); benchmarks with budgets on peak memory
   and on bytes stored and sent for a small edit to a large file.

## Open questions for review

Decided 2026-11-01 (see Done): Q1 over 16 MiB; Q2 1 MiB / 2 MiB / 4 MiB; Q3 a chunk
is a `ContentId`; Q4 the format takes 1 TiB. The options they were chosen from:
Q1 over 4, 16 or 64 MiB; Q2 256K/1M/4M, 1M/2M/4M or 64K/256K/1M; Q3 a
`ContentId` or a separate `ChunkId`; Q4 1 GiB everywhere, a larger limit
everywhere, or a per-repository setting.

- **Q2b — a manifest cut elsewhere.** A sender's manifest may not cut where this
  FastCDC would (another chunker, another version). The receiver:
  - (a) stores it as sent: the blob is the same, only its chunks share less with
    other versions;
  - (b) re-chunks it on receipt, reading the chunks back through the hasher it
    already runs, and stores its own manifest: every stored manifest is cut the
    same way.
- **Q5 — the chunker.** The `fastcdc` crate (its 2020 algorithm), or a small
  implementation of our own with the Gear table in the repository. Either way the
  boundaries are pinned by tests, since they are format.

## References

- `docs/design/server-native-source-control-store.md`, the paragraph on large
  files (whole-file `ContentId`, chunk manifests as physical layout).
- `docs/design/wheat-native-protocol.md`, content transfer (`ContentChunk`).
- Gothab: `crates/gothab-storage/src/object_store.rs` (`put_path`, multipart),
  `object_router.rs` (`spool_and_hash`), `crates/gothab-packages/src/oci/upload.rs`
  (upload sessions).
- FastCDC: Xia et al., "FastCDC: a Fast and Efficient Content-Defined Chunking
  Approach for Data Deduplication", USENIX ATC 2016.
- TODO.md: "The server reassembles a chunked upload in memory", "The size limits
  for one file disagree".
