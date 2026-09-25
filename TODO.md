# TODO

Open work. When an item is done, move it to `TODO_DONE.md` under the same heading, in
the same commit.

## Revise/compose cost at stack depth (2026-09-25)

Found with the span tracer (`WHEAT_TRACE_SUMMARY=1`, `WHEAT_TRACE=<file.json>`) on the
ignored `placing_an_edit_under_a_deep_stack_stays_fast` benchmark
(`crates/wheat-cli/tests/edit_move.rs`: 500 changes above B, 50 of them editing a 1 MB
file, debug build). `wheat add` takes about 5.1 s. Radix updates are about 2.1 s of
that. The budget passes, but by only about 0.7 s.

- `[ ]` **Measure a release build first.** All numbers above are from the debug test
  profile. Rerun the benchmark against a release binary to see which costs still
  matter.
- `[ ]` **Batch `apply_step`'s per-change map inserts.** `compose_authoring` does two
  single-item `radix_update`s per change (revision map, application map): 1002 calls,
  about 0.64 s. Build them in one pass per composition instead.
- `[ ]` **Batch `ViewLineageMapStore::insert_edges`.** One edge per parent means 503
  separate child-set `radix_update`s, about 0.6 s, in `finish_mutation`.
- `[ ]` **The four retained-set updates in `finish_mutation` cost about 0.85 s.**
  They are batched already; add spans inside `radix::Store::update_batch` to see why
  a batch of ~500 keys costs about 200 ms.
- `[ ]` **Why a single small `radix_update` costs about 0.6 ms.** Look inside it once
  the release numbers are in.
- `[ ]` **Move `WHEAT_IMPORT_PROFILE` (wheat-git) onto tracing spans**, so there is one
  profiler instead of two.

## Loose ends found in the design-doc audit (2026-09-25)

- `[ ]` **`wheat+tls://` URLs parse but cannot connect.** `crates/wheat-cli/src/remote.rs`
  accepts the scheme, but `tokio-tungstenite` is built with only the `connect`
  feature (no rustls/native-tls), in both wheat-cli and wheat-server.
- `[ ]` **The `.gitignore` fallback diagnostic points at a missing command.**
  `workspace_status.rs` tells users to run `wheat ignore import`, which does not
  exist (tracked in `wheat-local-tooling.md`).
- `[ ]` **Stale `#[allow(dead_code)]` markers in `workspace_walk.rs`.** They say
  "wired in the next step (status integration)", but `wheat status` already uses the
  walk.
- `[ ]` **The release-gate memory note lives under the gothab project.**
  `wheat-local-cli.md`'s release gate refers to
  `~/.claude/projects/-home-bart-Projects-gothab/memory/feedback_no_format_version_bumps.md`;
  agents working in this repo don't load it (the rule is in `CLAUDE.md` here anyway).

## Explicit history collapse, repack, and GC

Successive `add` operations intentionally retain immutable revision/view checkpoints
for one stable logical change. The latest revision is the change's complete current
meaning, so users may explicitly collapse unpublished checkpoints without changing
canonical edit identities or the final effective result. Ordinary native publish
preserves the exact local graph; it never compacts history implicitly. The settled
semantics and safety contracts are in `docs/design/wheat-local-cli.md` and
`docs/design/wheat-storage-efficiency.md`.

- `[ ]` **`wheat gc` (destructive).** Reclaim only bytes already unreachable from
  every authority in the backend's scope. Use exact fail-closed marking, transitive
  delta-base liveness, writer fencing, backend inventory, and reader grace periods;
  never decide history retention or collapse lineage.
- `[ ]` **Server-authoritative remote maintenance.** Mark all heads in a fork network
  for structural GC and all sharing domains for global content GC, publish replacement
  segments before retirement, and never let a client sweep shared storage from one
  repository closure.
- `[ ]` **Composed squash provenance for complex edit lineage.** Routine squash now
  preserves transition evidence when it can be represented exactly at the retained
  boundary and fails closed otherwise. Add an explicit composed-evidence representation
  before allowing collapse of checkpoint chains whose subsumption/resolution records
  refer to intermediate edits or applications that cannot survive verbatim.

Squash only makes old objects unreachable. Physical reclamation remains a later GC
operation; repack is independently useful and may run without either squash or GC.

- `[ ]` **Content repack still stops at 250,000 content IDs.** The format cap is
  gone (2026-09-24): a content generation is now a small index of
  16,384-entry shards, each with its own manifest and pack, so the on-disk
  format has no entry limit and readers load one shard at a time. What is left
  is the planner's work bound `ContentRepackLimits::max_content_ids` (250,000;
  the Gothab repo has 208,190). The planner now uses flat per-ID tables
  (~140 bytes per ID, 2026-09-24): at 1M IDs plan takes 8.3 s / 157 MiB and
  prepare 6.7 s / 137 MiB (were 15.6 s / 545 MiB and 21.8 s / 907 MiB). Pick
  the new bound after measuring a repository larger than Gothab. Those can't
  be imported yet:
  - aws-sdk-js-v3 has 14,330 refs, and the importer allows 1,024
    (`MAX_STAGE_REFS`, `wheat-git/src/staged_import.rs`).
  - Both aws-sdk-js-v3 and TypeScript have commits over the old 4,096 edits
    per revision (`TooManyEdits`): aws has 55 of 13,760 commits on main (the
    largest touches 137,525 files), and TypeScript has 25 of 31,611 (largest
    194,543). @warm-badger's change (2026-09-24) makes that limit per file,
    with a revision-wide default of 250,000 (`DEFAULT_MAX_REVISION_EDITS`),
    which covers both. Retry the imports once it lands.
  - Bare clones are in `/tmp/wheat-bigrepos/`.
- `[ ]` **Read-only `wheat` commands cost a full validation (~30 s on Gothab).**
  Measured 2026-09-24 on the disposable repacked import: `wheat labels` takes
  30 s / 350 MB RSS, the same as `wheat validate`, because
  `label_service::load` calls `RepositoryReader::validate_envelope` (full
  retained-history verification) just to list names. Profile is all tree and
  content-set decoding. Also, every `PackedObjectStore::open` re-reads and
  re-hashes the whole selected content pack (`validate_content_generation`,
  124 MiB) and tries one `unlink` per manifest entry
  (`retire_repacked_content`, 208,190 failing syscalls). Decide which commands
  truly need full verification and make store open O(changes), not O(store).
- `[ ]` **Remote packed store keeps its head pointer as JSON.**
  `crates/wheat-store/src/remote_packed/catalog.rs` `HeadPointer` is
  serde_json at `{prefix}/catalog/head`, which breaks the BinSchema-only rule
  for Wheat storage. The remote store also still runs on the
  `SegmentCatalogRecordV1` chain, while local stores now use only
  `PhysicalCatalogSnapshotV1`. Decide whether the remote store moves to
  snapshots too (one catalog format) and give the head a BinSchema record.
