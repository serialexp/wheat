# TODO

Open work. When an item is done, move it to `TODO_DONE.md` under the same heading, in
the same commit.

## Security and performance scan (2026-09-26)

Two read-only Antigravity scans of the whole repo, one for security and one for
performance. "Confirmed" means a Claude agent checked the claim in the code; the rest
are the scanner's word only and need checking before a fix.

Security, most serious first:

- `[ ]` **Non-Unix workspace reads follow symlinks (scanner only).** The
  `#[cfg(not(unix))]` path in `workspace_files.rs` (around line 825) checks
  `symlink_metadata` and then `File::open`s, which races and follows symlinks. Refuse
  non-Unix until it has a no-follow, descriptor-relative walk.

Performance, most serious first:

- `[ ]` **Fetch always sends the whole repository (confirmed).** `handle_fetch`
  (`crates/wheat-server/src/fetch.rs`) calls `collect_structural_closure`
  (`crates/wheat-store/src/closure.rs`) over all history, ignores what the client
  already has (`since_envelope`), and holds the closure in memory before sending.
- `[ ]` **Publish walks the whole content manifest (confirmed).** `handle_publish`
  (`publish.rs`, around lines 208–249) walks every content ID in the proposed
  envelope, even for a one-line change. It also asks for at most `MAX_WANT_IDS`
  (4,096) missing blobs, once; check whether publishes needing more than that fail
  (possible correctness bug).
- `[ ]` **Accidental O(E²) in staged-import verification (confirmed).**
  `verify_mapping` (`crates/wheat-git/src/staged_import.rs`, around line 700) does a
  linear `definitions.iter().find` per replay outcome. At 100,000 edits per commit that
  is 10¹⁰ comparisons. Index the definitions.
- `[ ]` **About four fsyncs per file during materialization (scanner only).**
  `execute_materialization` syncs the staged file, the install-intent progress record,
  the parent directory and the transaction for every file. Batch the intent records
  and directory syncs.
- `[ ]` **About four fsyncs per object when objects go in one by one (scanner only).**
  `apply_change_authoring` (`repository_mutation.rs`, around line 1045) and
  `handle_publish` (around lines 150–158) call `put_if_absent` in a loop, so each
  object is its own journaled active-segment commit. Use `put_batch`.
- `[ ]` **wheat-server does blocking disk work on async threads (scanner only).**
  Publication, packed-object and content stores are synchronous and called straight
  from Tokio tasks in `connection.rs`, `publish.rs` and `fetch.rs`. Move them onto
  `spawn_blocking` or give them async drivers.
- `[ ]` **`wheat status` re-reads and re-hashes every file (scanner only).**
  `workspace_scan.rs` has no saved stat cache (mtime, ctime, size, inode, content ID),
  so every status reads all tracked files and their baselines, and fails once the
  workspace passes `MAX_TOTAL_READ_BYTES` (1 GiB).
- `[ ]` **Uploads go one file per message (scanner only).** `publish_to_remote`
  (`remote.rs`, around line 950) sends each blob as its own `ContentUpload`. Add a
  batched upload message and stream large blobs to disk on the server.
- `[ ]` **One-by-one edit reads in import and authoring (scanner only).**
  `staged_import.rs` (around lines 674–691, 721–729) and `workspace.rs` (around line
  103) read edits with `reader.edit(id)` per edit and look paths up from the root one
  at a time. Use batch reads and `lookup_many`.
- `[ ]` **`MerkleTreeStore::apply_edits` re-reads the node it just wrote (scanner
  only).** `merkle_tree.rs` around line 1230 decodes the child node only to check
  whether it is empty. Return that from the recursive call instead.

## Revise/compose cost at stack depth (2026-09-25)

Found with the span tracer (`WHEAT_TRACE_SUMMARY=1`, `WHEAT_TRACE=<file.json>`) on the
deep-stack benchmark (then an ignored test, now the `deep_stack` case of
`wheat-bench quick`: 500 changes above B, 50 of them editing a 1 MB file; debug
build). `wheat add` takes about 5.1 s. Radix updates are about 2.1 s of that. The
budget passes, but by only about 0.7 s.

- `[ ]` **Measure a release build first.** All numbers above are from the debug test
  profile. Rerun the benchmark against a release binary to see which costs still
  matter (`just bench --case deep_stack`). First release numbers (2026-09-25,
  `bench/results/wheat-quick/20260925-052553-0581f94c5f96.tsv`): reapply 1.17 s,
  fitting 1.64 s, moving 1.52 s. The trace breakdown is still to do.
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
- `[ ]` **`descendant_frontiers` walks every view since the label basis (2026-09-26).**
  Switch, refresh, clone, init and `frontier list` find frontiers with a forward walk
  of the view lineage from the basis, so they are linear in the history since the
  basis. The lineage only stores parent → children links. Mutations no longer do this
  (see TODO_DONE.md); a fix here likely needs parent links or a frontier index stored
  with the label (option B from the mutation fix, a format change).
- `[ ]` **`checkpoint_stack` warm-up (2026-09-26).** A new store's revisions get
  slower over the first ~400 (single change: 1.5 → 2.6 ms, deep: 4.2 → 6.5 ms), then
  stay flat. The profile shows zstd decoding and malloc in the packed store; the cause
  is not proven.

## Loose ends found in the design-doc audit (2026-09-25)

- `[ ]` **`wheat+tls://` URLs parse but cannot connect.** `crates/wheat-cli/src/remote.rs`
  accepts the scheme, but `tokio-tungstenite` is built with only the `connect`
  feature (no rustls/native-tls), in both wheat-cli and wheat-server. Since
  2026-09-26 the client sends a bearer token from `credential.helper`, so over plain
  `wheat://` to another machine the token travels in clear text. Once TLS works,
  consider refusing to send a token over plain `wheat://` to a non-loopback host.
- `[ ]` **The repository registry is not pluggable (2026-09-26).** `Registry`
  (`crates/wheat-server/src/registry.rs`) is a fixed struct, and `main.rs` serves one
  repository. Access is pluggable now (`AccessProvider`), but an embedder such as
  Gothab also needs to look repositories up itself, and the standalone server has no
  way to serve several (`Registry::from_repos_dir` exists but nothing calls it).
- `[ ]` **Capturing `.git` paths is not gated (2026-09-26).** Writing paths inside
  `.git` now asks or needs `--allow-dot-git`, but `wheat add .git/config` captures
  one without a word. In a workspace that is also a git checkout that could put
  git's own files into the repository by accident. Decide whether capture should
  ask the same way.
- `[ ]` **The `.gitignore` fallback diagnostic points at a missing command.**
  `workspace_status.rs` tells users to run `wheat ignore import`, which does not
  exist (tracked in `wheat-local-tooling.md`).
- `[ ]` **The release-gate memory note lives under the gothab project.**
  `wheat-local-cli.md`'s release gate refers to
  `~/.claude/projects/-home-bart-Projects-gothab/memory/feedback_no_format_version_bumps.md`;
  agents working in this repo don't load it (the rule is in `CLAUDE.md` here anyway).
- `[ ]` **A content fetch can hang the client (2026-09-26).** `handle_content_fetch`
  (`crates/wheat-server/src/fetch.rs`) only logs (`debug!`) a requested blob it cannot
  read and sends nothing for it; its second fallback path (after a non-`TooLarge`
  bulk error) also drops blobs over 4 MiB instead of chunking them. The client waits
  for every requested blob, so it waits forever. The server should send an `Error`
  for the operation.
- `[ ]` **WebSocket messages up to 64 MiB are read before the 16 MiB check
  (2026-09-26).** Neither `accept_hdr_async` (server) nor `connect_async` (client)
  sets a `WebSocketConfig`, so tungstenite's default 64 MiB message limit applies and
  `decode_message` refuses the excess only after it has been read into memory. Set
  `max_message_size` to `MAX_MESSAGE_SIZE` on both ends.
- `[ ]` **Uploaded content is hashed four or five times on the server (2026-09-26).**
  Publish checks each blob's ID, then `put_many_with_bases`, `ContentStore::put_many`,
  the packed `put_many` and the loose writer hash it again; the packed `put_many` also
  clones every blob above the loose threshold. Hash once and pass verified content
  down.
- `[ ]` **The size limits for one file disagree (2026-09-26).** `wheat add` refuses
  files over 16 MiB (`DEFAULT_MAX_INPUT_BYTES`), Git import takes up to 64 MiB
  (`MAX_IMPORT_FILE_BYTES`), materialization up to 256 MiB, and the store and protocol
  up to 1 GiB. Decide which limit a file should have.
- `[ ]` **A failed seal after publish leaks its message (2026-09-26).** `handle_publish`
  turns a `seal` error into `ConnectionError::Protocol(Box::leak(...))`, leaking the
  formatted string each time. Use `ConnectionError::Internal`.

## Benchmarks (2026-09-25)

- `[ ]` **Run `wheat-quick` on Gothab end to end.** `gt profile wheat-quick` needs the
  `serialexp/wheat` mirror, which still showed `mirror_status: "failed"` and 0 commits
  on 2026-09-25. Once it syncs, check that the run passes on `pt-default`, and how
  long it takes there (5 min on a Ryzen 9 9900X).
- `[ ]` **Building 505 changes takes 127 s.** `transition_validate.build.wall` in the
  first `wheat-quick` results: `Repository::create_change` plus two publishes,
  about 250 ms per change in release. Trace where the time goes; it is also 40% of
  the suite's runtime. Lead (2026-09-26): profiling a 20,000-revision in-memory build
  showed most time in `String`/`BTreeMap<String, FileValue>` and `Change` clones, so the
  in-memory `Repository` seems to copy the whole tree and change set per revision.
- `[ ]` **Tighten the quick contract rules.** The budgets came from debug builds (for
  example `deep_stack.*_overhead ≤ 1.5 s`, measured 0.35–0.47 s in release, and
  `add_scale.*.wall ≤ 30 s`, measured at most 5.5 s). Set them from a few `pt-default`
  runs instead.

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
