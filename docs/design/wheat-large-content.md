# Wheat large content — Design

Status: partial — steps 1–7 and the stat cache with sampled status landed; recording the cache on materialize and add outstanding
Owner: bart
Last updated: 2026-11-06

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

- [x] **Decision — a manifest cut elsewhere (Q2b, 2026-11-01).** Stored as sent.
- [x] **Decision — the chunker (Q5, 2026-11-01).** Our own FastCDC in
  `wheat-store`, with its Gear table in the repository.

- [x] **Decision — a two-level manifest (Q6, 2026-11-01).** At the 1 MiB
  minimum a 1 TiB blob's flat manifest reaches 36 MiB, more than one 16 MiB
  protocol message. Bart: "do a two level manifest, but maybe keep the manifest
  at the first level as something that also contains content items so smaller
  files only need the single manifest". See [Two levels](#two-levels). The
  options were the manifest sent in pieces, larger chunks for larger files (in
  tiers or scaled with the length), or two levels.
- [x] **Decision — where a part ends (Q7, 2026-11-01).** Content-defined, like a
  chunk: 1,024 to 16,384 chunks, about 4,096 on average, so an insertion rewrites
  only the parts around it. The other option was a fixed count of chunks per
  part, where an insertion near the start moves every later part.
- [x] **Decision — how many chunks a manifest lists itself (Q8, 2026-11-01).**
  4,096, about 8 GiB at the 2 MiB average; a direct manifest is at most 144 KiB.
  The options were 4,096, 16,384 or 65,536.
- [x] **Decision — what a receiver holds between a manifest and its check (Q9,
  2026-11-01).** One answer carries at most 64 MiB of manifests, and always at
  least one; the receiver asks again for what was left out, and checks the
  manifests it holds before asking for anything new. The options were that cap, a
  temporary file per operation, or one manifest per answer.

- [x] **Step 1 — format.** The manifest record `ContentChunkManifestV1` with
  canonical validation (`wheat_core::ContentChunkManifestRecord`), and the
  chunker (`wheat_store::ContentChunker`), its Gear table and cut points pinned
  by tests. The `Chunked` codec number (5) landed with step 2, where the store
  first reads and writes it. Extended beyond the plan with the two levels (Q6–Q8):
  `ContentManifestEntries` (chunks or parts), `ContentChunkPartRecord`
  (`ContentChunkPartV1`), and part cutting in `wheat-store/src/content_manifest.rs`,
  its boundaries pinned by a test.
- [x] **Step 2 — store.** `ContentStore::writer` and `ContentStore::reader`
  (`wheat-store/src/content_stream.rs`) over loose files, the packed store and
  object-storage blobs; `put`/`put_many` store a blob over the threshold as
  chunks too, and `get`/`get_bounded` assemble one that fits their bound. Beyond
  the plan: the packed engine writes chunks loose, through its loose-file
  bookkeeping, before the manifest that names them; content repack carries a
  manifest as it is and its chunks as ordinary content, never reading the blob
  whole; a chunked blob is never a delta or a delta's base. Tests count the bytes
  writer and reader hold (`tests/content_stream.rs`); bench case
  `content_large_blob` (1 GiB, 2026-11-01, under a load average of about 22):
  write 5.5 s holding 16 MiB, a one-byte edit 0.8 s writing 2.7 MB, read 0.4 s
  holding 4 MiB, peak RSS 58 MiB.
- [x] **Step 3 — protocol.** Manifest-first transfer of chunked blobs in both
  directions: `ContentManifest` (0x34) in place of a blob over the threshold,
  chunk-level have/want through `ContentNeeded` and `ContentFetchRequest`, and a
  receiver (`wheat_store::ManifestArrivals`) that asks for missing parts, then
  missing chunks, checks the blob through `ContentStore::check_manifest` and stores
  the manifest with `RepositoryStage::put_manifest`. Senders send a
  long blob's stored manifest without reading the blob. Beyond the plan:
  `ContentFetchEnd` (0x35) ends every fetch answer, since a fetch answer may now
  leave blobs out ([Follow-up](#follow-up-step-3-as-built)); the `ContentChunk`
  limit is 16 MiB at both ends. Wire tests in
  `wheat-server/tests/clone_fetch_publish_test.rs` and `fetch_test.rs` cover a
  publish, a one-byte edit that asks only for the changed chunks, a manifest left
  out and asked for again, refusals, and a fetch. The 64 MiB answer budget is
  tested at the unit level only (`ManifestBudget`); publish and fetch benchmarks
  are step 7.
- [x] **Step 4 — client.** `wheat add` streams a file over 16 MiB into
  `ContentStore::writer` (6072e1a), and edit application confirms whole-file content
  instead of reading it (0e918a5). Materialization authenticates leaves by preimage,
  whose digest is now the content ID's, so expected leaves come from stored lengths, and
  stages files through `ContentStore::reader` with a 64 KiB buffer; a batch is bounded
  by what its readers hold, `ContentReader::held_bound` (5a0ffbe; `publish_content`
  clone peak 101 MiB, was 120 MiB). Edit move places a long file whole without reading
  it (`edit_placement::place_long`, `WholeFileReason::Long`) instead of refusing it
  (f414ca8). Publishing confirms fresh content rather than resolving it, and a conflict
  on a long file resolves as one whole-file edit from the values
  (`edit_capture::capture_whole_file`, 6a34681). A guarded edit's long target is
  refused from its length before it is read (`ContentSource::logical_length`,
  4909be5). Diff, status and `cat` already read at most their bound: `get_bounded`
  checks the stored length before reading, so a longer blob is described by its ID and
  length (diff, status) or refused (`cat`).
- [x] **`cat` past its bound.** Decided 2026-11-05: `wheat cat` keeps refusing
  content over its 8 MiB output bound (just under 6 MiB in JSON) and names
  `--no-limit`, which streams through `ContentStore::reader` with no bound. JSON
  output is base64 (`result.base64`) instead of a byte array; with `--no-limit` it is
  JSON lines of 3 MiB base64 parts ending in `{"ok": true, "parts": n}`.
- [x] **Decision — reading long Git blobs (2026-11-05).** Bart: reuse Gothab's
  approach, whose remote helper streams `git cat-file blob <oid>`. gix reads an
  object only whole, and Wheat already runs `git` to push.
- [x] **Decision — what the importer knows about a blob it stored (2026-11-05).**
  Bart: "keep them in sqlite and fall back to A": the import journal maps a long
  blob's Git ID to the `ContentId` it was stored as, and without a row the blob is
  read and hashed again.
- [x] **Step 5 — Git bridge.** Export writes a blob over 16 MiB to Git with
  `gix::objs::Write::write_stream` from `ContentStore::reader`, charging its stored
  length to the export limit (`export.rs`, `write_blob`). Import takes a side's
  length from the Git object header; when either side is longer than the guarded
  input bound plus `MAX_IMPORT_GUARDED_LINES` bytes (`STREAMED_BLOB_BYTES`, just over
  16 MiB) it records the one whole-file replacement capture would, from the values
  alone (`staged_import/tree_diff.rs`, `record_whole_file`). A side's value comes
  from the journal's `blob_content` table for a before side, else from streaming
  `git cat-file blob` through `StreamingCapture`, and the blob is streamed a second
  time, canonicalized by `LineEndings`, into `ContentStore::writer` when the store
  lacks it. `wheat pull` has no journal and always hashes. The 64 MiB import file
  limit no longer meets these blobs. Tests: `a_streamed_side_is_one_capture_records_whole`
  (the bound is tight), `long_blobs_stream_into_whole_file_edits_and_map_their_content`
  (wheat-git), `long_files_round_trip_through_git` (push, pull and import of 17 MiB
  files, wheat-cli).

- [x] **Step 6 — repack, verification, bundles.** Repack already met it in step 2:
  it carries a manifest as it is, its chunks and parts as ordinary content, and never
  makes a chunked blob a delta or a delta's base, and content generations hold them
  (`a_packed_store_keeps_chunked_blobs_through_a_repack`, 72 MiB). Full verification
  reads a chunked blob a chunk at a time through `ContentStore::verify`
  (`check_manifest`) instead of assembling it and hashing it twice. Network bundle
  export and import, and adopting a repository, copy a blob over the threshold with
  `ContentStore::copy_to`, a reader into a writer, instead of batching it whole and
  cutting it again. Tests: `verifying_a_chunked_blob_reads_every_chunk`,
  `a_chunked_blob_copies_into_another_store_as_chunks`,
  `a_long_blob_moves_through_a_bundle_as_chunks`. What each holds is step 7's to
  measure.

- [x] **Bench children measured from a fresh launcher (94ac059).** Found while
  writing step 7: a child's `wait4` peak counts the memory it was forked from, so
  every client command after an in-process server grew reported the case's size
  (`edit_add` 164 MiB, really 82 MiB). Bart chose a launcher process.
- [x] **Decision — the tools' file limit (2026-11-05).** Bart: 256 MiB stays the
  default, "just there to stop footguns, not an actual limit", raised by a tracked
  `.wheat.json` at the workspace root (not in `.wheat/`, which is never tracked);
  `<store>/config.json` stays the local, untracked configuration. See
  [Tool limits](#tool-limits-and-status-on-long-files).
- [x] **Step 7 — benchmarks and contracts.** `large_file`: add, publish, clone,
  a one-byte edit added, published and fetched, and refresh, which materializes it;
  every process stays under 192 MiB (highest 153 MiB at 1 GiB) and the edit stores and
  sends under two chunks and 1 MiB (2.5 MiB measured). `large_file_git`: push, a
  one-byte edit pushed, and a fresh import, which stores the second version as the
  chunks it changed (1026.6 MiB for both versions of 1 GiB). Found and fixed on the
  way: the server read a whole content-fetch request before answering (clone server
  peak 1222 → 153 MiB, 69b5261) and a symlink read allocated its whole limit
  (4d874fe).
- [x] **Decision — Git's own peaks (2026-11-06).** Bart: if it is how Wheat calls
  git, fix it; if it is what git does, leave it. Plain `git push` and `git cat-file`
  of a 1 GiB blob peak at 1028 MiB without Wheat (git maps the object file), so
  `large_file_git` records the Git steps' peaks without a budget.
- [x] **Decision — status on long files (2026-11-05).** Bart: a stat cache, then
  samples: an unchanged stat means unchanged; a changed stat with different samples
  means modified; matching samples get a full hash.
- [x] **Stat cache and sampled status.** Same section. Decided 2026-11-06: the
  cache is SQLite (Bart: "cache goes in sqlite. Durable goes in binschema"), and
  samples are 256 of 4 KiB. When a long file's stat changed, human status reports it
  modified on differing samples without reading the rest, and hashes it in full when
  they match; `--format json` hashes it in full either way, so its content ID is
  always there (Bart, after the first answers contradicted each other). Built as
  `.wheat/stat.sqlite` (`wheat-client/src/stat_cache.rs`), recorded by status
  (`workspace_scan.rs`, `ContentIds::{Required, Optional}`); see the follow-up under
  [Tool limits](#tool-limits-and-status-on-long-files). Tests in
  `wheat-cli/tests/stat_cache.rs`; bench case `status_scale` (release, 2026-11-06,
  20,000 small files and a 256 MiB one): the first status reads all 20,002 files in
  0.63 s, a status with nothing changed reads none in 83 ms, ten edited files are the
  ten read, and a changed sample of the long file is found by reading its samples
  alone. Found on the way: each stat opened every directory above the file, and the
  untracked walk stat'd every entry for its type; both now cost no extra syscall.
- [x] **Status on files over 1 GiB (2026-11-06).** Status still charged every read to
  a 1 GiB total and gave each file what was left as its limit, so a file over 1 GiB
  failed status, as did a first status over 1 GiB in all. Bart: status should work
  on files above 1 GiB. The cap is gone: status streams each file, so a file is
  limited only by the format's 1 TiB. `status_scale` now has a 1.25 GiB file at full
  scale: the first status reads 1.43 GB in 0.80 s, hashing the long file in full
  takes 0.37 s, and the case peaks at 101 MiB (budget 192 MiB).

### Outstanding

- [ ] **Defer — snapshot export.** `export_snapshot_to`, used by tests and a bench case
  only, sends content whole and refuses a blob over its 8 MiB batch.
- [ ] **Defer — the in-memory Git importer.** `wheat_git::import_repository`, used by
  tests and an example only, still reads every blob whole and refuses one over the
  guarded bound.
- [ ] **`.wheat.json` file limit.** Default 256 MiB, at most the format's 1 TiB; see
  the open points under [Tool limits](#tool-limits-and-status-on-long-files). Built
  on provisional answers (Bart had not answered the open points by the time it
  was needed): the limit applies to capture only, so status and materialization
  take up to 1 TiB; add reads `.wheat.json` from disk
  (`repository_config::read`). Not built: a local override in `<store>/config.json`.
- [ ] **The stat cache on materialize and add.** Status records what it reads;
  materialize and add, which write or read the same files, record nothing yet, so the
  first status after them reads every file they touched. Needs the racy-clean rule
  where Wheat itself wrote the file.

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
sharing. A receiver stores a manifest as sent (Q2b).

The chunker's parameters are part of the format: changing them later changes where
new versions cut, and so how much they share with old ones, though never what any
`ContentId` names.

### Two levels

Added 2026-11-01 (Q6–Q8), after the flat manifest above turned out to outgrow a
protocol message. A blob of up to 4,096 chunks (about 8 GiB) keeps the flat
manifest. A longer one groups its chunks into **parts** and its manifest lists the
parts instead:

```
ContentChunkManifestV1 {
  logical_length: uint64
  chunks: array<ContentChunkRefV1>  // a blob of up to 4,096 chunks; else empty
  parts: array<ContentPartRefV1>    // a longer blob's parts, in order; else empty
}
ContentPartRefV1 {
  id: ContentId                     // the part, stored as ordinary content
  length: uint64                    // the blob bytes its chunks hold
}
ContentChunkPartV1 {
  chunks: array<ContentChunkRefV1>  // 1 to 16,384; at least 1,024 but in the last
}
```

Exactly one of `chunks` and `parts` is non-empty. A part is content: its ID is
the `ContentId` of its encoding, it is stored and sent like any blob (at most
576 KiB, within a `ContentBatch` item), and a receiver checks it as it checks any
blob. A part ends after a chunk whose ID's first eight bytes, read as a
little-endian number, are a multiple of 3,072, once it holds 1,024 chunks, or at
16,384 (Q7). The cut depends only on the chunk IDs, so an insertion changes the
parts around it and no others.

A reader walks the manifest a part at a time, holding one part's chunk list
(at most 576 KiB) and one chunk. Validation of the top level checks the parts'
lengths: each but the last holds at least 1 GiB (1,024 chunks of 1 MiB) and at
most 64 GiB. A part is checked against its entry when it is read: the count of
chunks, their sizes, and that they hold the entry's length.

A manifest is now at most 144 KiB (4,096 chunks) and a 1 TiB blob's at most
40 KiB (1,024 parts), so the [Size](#size) figures below for a flat manifest no
longer apply to the stored record.

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

### Follow-up: step 3 as built

The steps above held, with these details settled while building it (2026-11-01):

- **Messages.** `ContentManifest { op_id, content_id, manifest }` (0x34) carries
  the encoded `ContentChunkManifestV1` as stored. `ContentFetchEnd { op_id }`
  (0x35) ends a fetch answer: once an answer may leave blobs out (the manifest
  budget below), the client can no longer count items to know it is over.
- **Answers.** One answer, a publish's reply to `ContentNeeded` or a fetch's to
  `ContentFetchRequest`, carries at most `MAX_ANSWER_MANIFEST_BYTES` (64 MiB) of
  manifests, and always the first (Q9). A sender leaves the rest out; the receiver
  puts the IDs it was not sent back at the front of its queue.
- **Rounds.** Before asking for anything new, the receiver asks for the parts its
  held manifests name and then for their missing chunks, in rounds of at most
  `MAX_WANT_IDS`, each answered in full; a manifest in such a round is refused.
  Once nothing is missing it checks each blob against its ID and stores its
  manifest as one job, then goes back to its queue.
- **Refusals.** A publish is refused when a manifest's chunks are not the blob
  ("does not match its chunks"), when a chunk is sent as a manifest, when a blob
  over the threshold comes as bytes, and when a round is answered with nothing.

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

  As built (step 5): gix has no streaming object reader, so import streams a long
  blob from `git cat-file blob` instead, as Gothab's remote helper does. A transition
  with a long side is a whole-file replacement either way, so import records it from
  the two values without capture.
- **Repack** treats chunks as ordinary content (delta-free) and manifests as small
  records; its 64 MiB per-record limit no longer meets large blobs.
- **Verification and bundles** (`repository_verify`, network export/import) read
  through the reader and copy chunk by chunk.

## Tool limits and status on long files

Below the format's 1 TiB, two tool limits remained when step 7 began, both 256 MiB
per file: `MAX_WORKSPACE_FILE_BYTES` (add and status) and materialization's
`MAX_FILE_BYTES`. Both paths stream, so neither bounds memory, only work. Status also
stops after 1 GiB of reads in one scan (`MAX_TOTAL_READ_BYTES`), because it hashes
every tracked file on every run: one 1 GiB file and anything else, and status fails.

**The file limit.** `.wheat.json` at the workspace root is tracked like any file:

```json
{ "format": "wheat-repository", "version": 1, "max_file_bytes": 4294967296 }
```

Without it the limit is 256 MiB; `max_file_bytes` above 1 TiB
(`MAX_CHUNKED_CONTENT_BYTES`) is refused. Open:

- Where the limit applies: when a workspace file is captured (add, and the capture
  publish and edit capture do), or also when materializing and in status. Content
  already in the repository got past someone's limit.
- Which `.wheat.json` add reads: the one on disk, so raising the limit and adding the
  file can be one change, or the one in the change's basis.
- Whether `<store>/config.json` may override it locally, and in which direction.

**Status.** A stat cache in `.wheat/` keeps, per tracked path, the size, mtime,
ctime, inode and mode it had when Wheat last wrote or read it (materialize, add,
status), with its content ID. Status reads a file only when its stat changed. As
Git's index does, an entry whose mtime is not older than the cache's own write is
racily clean and counts as changed. A file over the chunking threshold also keeps a
digest of fixed samples of it; when its stat changed, status reads the samples
first: different means modified, without reading the rest, and the same means a
full hash. Open:

- The cache's format: SQLite (`.wheat/stat.sqlite`), updated row by row, or a
  BinSchema file rewritten whole when it changes.
- The samples: how many, how large (for example the first and last 4 KiB and 62
  evenly spaced 4 KiB blocks, 256 KiB read for any length).
- A modified long file found by its samples has no content ID until add reads it:
  status reports its workspace side without one (`DiffDescriptor::content` becomes
  optional) or hashes it anyway when asked for IDs (`--format json`).

Follow-up (2026-11-06): all three were decided as listed under Done (SQLite; 256
samples of 4 KiB; human status without an ID, JSON with one), and status is built.
The cache loads whole when a read starts, since status looks up every tracked path.
An entry is kept only when the file's mtime is older than a time the filesystem gave
at the start of the read (the mtime of `.wheat/stat.clock`, written then), and only
when the file's stat was the same before and after it was read. The samples are a
blake3 digest of the length and 256 blocks of 4 KiB at evenly spaced offsets, first
and last included. Only binary files over the chunking threshold keep them: a text
file's content ID is of its canonical text, so its raw bytes could change (line
endings) while its content does not, and human and JSON status would disagree.

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

None open. Decided 2026-11-01 (see Done): Q1 over 16 MiB; Q2 1 MiB / 2 MiB /
4 MiB; Q2b store as sent; Q3 a chunk is a `ContentId`; Q4 the format takes 1 TiB;
Q5 our own chunker. The options they were chosen from: Q1 over 4, 16 or 64 MiB;
Q2 256K/1M/4M, 1M/2M/4M or 64K/256K/1M; Q3 a `ContentId` or a separate
`ChunkId`; Q4 1 GiB everywhere, a larger limit everywhere, or a per-repository
setting; and Q2b and Q5 as below.

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
