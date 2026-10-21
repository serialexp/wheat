# TODO

Open work. When an item is done, move it to `TODO_DONE.md` under the same heading, in
the same commit.

## Found while dogfooding (2026-10-19)

A release build imported Wheat's own history (163 commits), pushed changes to a bare
Git remote, and ran two clones against a `wheat-server` with a conflict between them.
Import, Git push (incremental, fast-forward), clone, publish, fetch, pull and
resolve worked; a fetched join with a conflict failed validation (fixed in 3485b1e).
Besides labels never moving (`docs/design/wheat-label-moves.md`), plain-text
`wheat://` (TLS item below) and read-only commands paying a full validation (1.4 s
for `wheat labels` here; incremental-validation step 9):

- `[ ]` **`wheat push --force-with-lease` counts every exported commit as pushed.**
  Its report says "pushed N commits" for the whole exported chain, not the commits
  the remote lacked.
- `[ ]` **Git push and pull re-project a whole tree each time (2026-10-21).** Push
  checks its reused anchor, and pull a commit it recognises as pushed, by writing the
  Wheat tree into the projection again (`export.rs` `project_tree`/anchor check), so
  each reads every file of the tree once: O(files), not O(changes). A persisted
  Wheat-tree → Git-tree map in the projection would make both O(changed paths).
  `git_pull_incremental` records the cost.
- `[ ]` **Losing the Git projection duplicates pushed changes on the next pull
  (2026-10-21).** The commit refs in `projections/git.git` are the only record of
  which Git commit a native change was pushed as, so a store without them imports
  its own pushed commits as new changes beside the originals.
- `[ ]` **A rebased change the pulled commits already contain is kept as an empty
  change (2026-10-21).** Pull moves every local change above the pulled commits; one
  whose edits are already there (the same fix made in Git) is not dropped as
  `git rebase` would, so the next push projects it as an empty commit. Untested.
- `[ ]` **`wheat status --format=json` prints view IDs as byte arrays (2026-10-21).**
  `basis`, `effective_view` and the other view fields serialize as 32-number arrays;
  every other command prints them as hex.
- `[ ]` **A native pull or fetch without `remote.json` is an internal error
  (2026-10-21).** `resolve_remote` (`main.rs`) fails with plain context ("no
  remote.json found; use wheat clone or pass --remote"), so `wheat pull wheat://...`
  in an imported store reports `internal / unexpected_failure`, and the advice to
  pass `--remote` doesn't help: an explicit remote also needs `remote.json`.
- `[ ]` **Resolving a conflict is awkward.** The workspace shows only one side of a
  conflicted file, with no markers, and `wheat resolve` takes the whole resolved
  file as `--value` on the command line (bounded by the argument size limit). After
  resolving with exactly the content already in the workspace, `wheat materialize`
  refuses ("changed from its exact observed value") and `status` keeps listing the
  file as modified until `wheat restore` of that path. Options: resolve from the
  workspace file, and treat a workspace file equal to the new view's as unchanged.
  Root causes (2026-10-21): `resolve` publishes a new view but never moves the
  materialized pin (`add` does, through `capture_settlement::settle`), so `status`
  compares with the old side; `materialize` refuses only when the bytes match neither
  side, which `--value "$(cat f)"` causes by stripping the trailing newline; and
  `restore` then brings back the old side, not the resolution.
- `[ ]` **Resolving one path of a multi-path conflict drops the rest (2026-10-21).**
  `resolve_conflict` removes every failed edit of the conflict
  (`crates/wheat-store/src/repository_mutation.rs:415`, core `repository.rs:963`), and
  the CLI takes a single `--path`, so the change's edits on its other conflicted paths
  vanish silently and those files keep the other side's value.
- `[ ]` **Resolution revisions have no author or date (2026-10-21).** They use
  `RevisionMetadata::default()` (`repository_mutation.rs:445`), so `wheat log` shows
  `author  <> 1970-01-01T00:00:00Z`; integrate's combine-by-date order may see it too.
- `[ ]` **`wheat conflicts` prints Rust debug output.** The human rendering shows
  `target=Some(FileValue { content: ContentId([210, 80, ...]), kind: Regular })` and
  `reason=NoMatch`; and it names the conflict and revision by ID only, not by change
  message or path summary.
- `[ ]` **`wheat add <directory>` adds nothing (2026-10-20).** With `many/00/1.txt`
  and `many/00/2.txt` untracked, `wheat add many` reports "added 1 path(s)" and makes
  a revision, but `status` still lists every file under `many/` as untracked. Either
  add the directory's files or refuse a directory by name. Found with it
  (2026-10-21): the revision holds only the predecessor's edits (`preserved=1
  added=0`); any add that changes nothing (a missing path, an unchanged file) still
  publishes a revision; "added N path(s)" counts the arguments, not captured paths;
  and `wheat add .` / `wheat add many/` fail as an internal error (`invalid directory
  component`), because `lookup_many` runs before the paths are validated.
- `[ ]` **`wheat log` lists changes in ChangeId order.** For imported history the
  IDs are UUIDv5, so the log is in effectively random order, and it prints every
  change with its whole message, with no limit or paging. Order by the view's
  canonical order (or time), newest first, with a count limit.

## Found while scoping the Scion lessons (2026-10-15)

See `docs/design/wheat-lessons-from-scion.md` for the planned work; these are gaps
between `wheat-native-protocol.md` and the code.

- `[ ]` **Content fetch checks no reachability.** `handle_content_fetch`
  (`crates/wheat-server/src/fetch.rs`) serves any `ContentId` in the repository
  entry's content store. The design says content is served only when the client's
  authorized graph references it. Check whether a content store can be shared between
  repositories; if so this leaks content across them.
- `[ ]` **Protocol doc claims chunk-level resume.** `wheat-native-protocol.md`,
  "Resolved design questions", says transfers resume from the last received chunk.
  Nothing does; a partly received chunked blob is fetched again. Correct the doc or
  implement it (tracked as "Resumable — content offsets" in the Scion doc).
- `[ ]` **Delta content on the packed store bypasses the one append.** On
  `PackedObjectStore`, delta-encoded content is written as one loose envelope file per
  blob (`PackedEngine::put_raw_envelopes`, and `put_publication`), each with its own
  file and directory syncs, outside the catalog append. Each modified file of a
  publish or fetch therefore costs extra fsyncs; `publish_contention` only adds new
  files, so it does not show this. Options: store deltas as segment records, or skip
  deltas on the durable write path and leave them to repack. Measure with a case that
  modifies files first.
- `[ ]` **A crash while retiring a sealed active segment duplicates its records.**
  `seal_active` publishes the catalog snapshot naming the sealed segment, then
  `retire_active` renames an empty log over `active.segment`. A crash in between
  reopens the old log, so every record is in the sealed segment and in the active one,
  and the next seal writes them into a second sealed segment. Reads are unaffected;
  the space is lost until a repack. The removed `active.head` check meant to catch
  this compared the active segment's ID with the sealed one's, which never matched
  once sealing rebuilt the records with block compression. Fix: record in the log
  which catalog generation sealed it, or retire before publishing.

## Security and performance scan (2026-09-26)

Two read-only Antigravity scans of the whole repo, one for security and one for
performance. "Confirmed" means a Claude agent checked the claim in the code; the rest
are the scanner's word only and need checking before a fix.

Security, most serious first:

Performance, most serious first:

- `[ ]` **`existing_ids` lists the whole S3 bucket even for a few IDs (scanner
  only, 2026-10-05).** `ContentStore::existing_ids` (`content_store.rs`) uses
  `list_ids` whenever the backend has it, so a publish of one file lists every blob.
  Choose per-item checks below some count.
- `[ ]` **Read-only commands still sync (2026-10-05).**
  `PackedCatalog::open_with_target` (`packed/catalog.rs`) calls
  `sync_created_ancestors` on each open, which syncs every directory from
  `storage/packed` up to `/`, whether or not it created anything. Measured on NVMe
  ext4: the extra ancestors cost about 2 µs each; the first sync of a run costs
  2–9 ms because it flushes the drive's cache of everything the system wrote
  since the last flush (a bare `fsync` of an untouched directory does the same).
  So `wheat status` pays one drive flush. Re-syncing on open also makes durable a
  directory an earlier, interrupted open created, so dropping it for read-only
  commands needs a decision: only sync what this open created, or sync only before
  the first write.
- `[ ]` **Materialization still syncs every staged file (2026-10-05).** After
  batching, a switch replacing 20,000 files takes about 6.4 s, about 4 s of it the
  staged files' own fsyncs (32 at a time). One `syncfs` per batch would replace
  them, but it flushes the whole filesystem, so on a busy disk it may cost more.
  Needs Bart's call; measure with bench `materialize_wide`.
- `[ ]` **wheat-server does blocking disk work on async threads (confirmed
  2026-10-05).** Publication, packed-object and content stores are synchronous and
  called straight from Tokio tasks in `connection.rs`, `publish.rs` and `fetch.rs`
  (about 20 sites); only the S3 store uses `block_in_place`. The CLI runs a
  current-thread runtime, where `block_in_place` panics, so it cannot simply go
  inside the shared stores. Options put to Bart: `block_in_place` at the server's
  call sites; `spawn_blocking` inside the stores (a handoff per get); or each
  handler's store work as one blocking job talking to the connection over channels.
  Since 2026-10-16 the publish committer (`commit.rs`), which does the store write and
  the head's syncs, runs on the blocking pool. What is left on the runtime are reads
  and the stage's early writes; in `publish_contention` at 8 writers the server is
  about 5% busy, so that case cannot show their cost.
- `[ ]` **`wheat status` re-reads and re-hashes every file (scanner only).**
  `workspace_scan.rs` has no saved stat cache (mtime, ctime, size, inode, content ID),
  so every status reads all tracked files and their baselines, and fails once the
  workspace passes `MAX_TOTAL_READ_BYTES` (1 GiB).
- `[ ]` **The server reassembles a chunked upload in memory (scanner only).** A blob
  that comes as `ContentChunk`s is held whole until its last chunk, up to 1 GiB
  (`MAX_CONTENT_SIZE`). Stream large blobs to disk instead.
- `[ ]` **Re-adding 20,000 files to one change takes 5.4 s (2026-10-05).** Release,
  NVMe, `wheat add` of 20,000 modified files to a change that already edits them
  all (`WHEAT_TRACE_SUMMARY=1`): `persist_edits` 1.6 s, the first
  `apply_revision`'s `content_commit` 1.2 s, `capture_path` 0.7 s with six
  `content_get`s per path, and each `apply_edits` reads every content blob again
  (60,000 `content_get`s, twice). Bench `add_scale` adds its second round to a new
  change, so it doesn't measure this. Since 2026-10-20 (01013f1) `apply_edits` no
  longer re-hashes what it resolves, but it still reads every blob; not re-measured.
- `[ ]` **Content hashed twice where hash-once didn't reach (2026-10-20).** Left over
  from the hash-once work (`CURRENT_TASK.md`):
  - the packed catalog's duplicate-put check re-reads and re-hashes the record
    already stored;
  - remote_packed seal validates objects (not content) twice: at `put_if_absent` and
    again through `CheckedRecord::check` in `seal`;
  - `FileBytes::value` in `crates/wheat-cli/src/edit_move.rs` hashes on every call
    during `wheat move` replay.

  Not a defect: `wheat add` hashes raw file bytes in the scan and canonical
  (newline-normalized) bytes in capture, which can differ.

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

- `[ ]` **Full validation and the mutator disagree on conflict resolutions
  (2026-10-21).** A resolution must name exactly the conflict's failed edits in full
  validation (`repository_verify.rs`, `!=`), but only some of them in
  `Repository::validate_edit_transition` (`wheat-core` `repository.rs`, `is_subset`).
  Full validation also doesn't check that the resolved application is one of the
  predecessor revision's, which both the mutator and wheat-core require. Bart to pick
  the rule; then make all three agree before incremental
  validation step 3 copies full validation's.
- `[ ]` **Workspace reads keep their own libc module (2026-10-05).** wheat-cli's
  other operating-system calls now go through `sys.rs`, but `workspace_files.rs` still
  has a `#[cfg(unix)] mod platform` that calls `openat`, `fstatat` and `readlinkat`
  on raw descriptors, beside a non-Unix twin that refuses everything. Moving it onto
  `sys` (open, describe and read-link at a directory) would leave one place for
  platform code. `discovery.rs` (the device that stops discovery at a mount) and
  `store_analyze.rs` (allocated bytes) also keep small `cfg(unix)` branches, whose
  non-Unix halves guess instead of failing.
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
- `[ ]` **The release-gate memory note lives under the gothab project.**
  `wheat-local-cli.md`'s release gate refers to
  `~/.claude/projects/-home-bart-Projects-gothab/memory/feedback_no_format_version_bumps.md`;
  agents working in this repo don't load it (the rule is in `CLAUDE.md` here anyway).
- `[ ]` **`status` lists an untracked directory and every file in it (2026-10-09).**
  A new `newdir/a` shows as `untracked newdir/` and `untracked newdir/a`, and the
  header counts both as "untracked files: 2". Options: collapse to the directory
  as git does, list only files, or keep both and count only files.
- `[ ]` **The fetch walk has no cap on repeated radix subtrees (2026-09-26).** A
  radix node's ID does not include its key path, so one node can sit under several
  edges, and a crafted map can repeat that at every level: a few nodes, walked 2^depth
  times. `StructuralDelta` (`crates/wheat-store/src/structural_delta.rs`) walks a node
  once per (node, role, path) and keeps every such triple in `radix_walked`, so both
  its time and its memory follow the repeats. `max_pending` does not help: the queue
  stays short. Snapshot's `ClosureWalk` caps visits at twice `max_objects`
  (`LimitExceeded("closure visits")`); the fetch walk needs a similar cap. Check
  first which maps a client can fill with chosen keys (change IDs are UUIDs the
  client picks). 2026-10-09: the fetch union (`crates/wheat-store/src/repository_union.rs`)
  diffs the remote's change map, lineage and retained sets against the local ones
  with `DiffLimits::unbounded()`, so a server could do the same to a client; a
  visit cap there too.
- `[ ]` **Combined revisions carry no subsumption evidence (2026-10-09).** When
  integrate combines revisions of one change, the new revision's transition has no
  `subsumed` or `resolutions` entries, even where the combined revisions recorded
  them. Check whether `later_edits` / path history need them carried over.
- `[ ]` **JSON output of `status` and `conflicts` gives IDs as byte arrays
  (2026-10-09).** `WorkspaceStatus` (`crates/wheat-cli/src/workspace_status.rs`) and
  `ConflictEntry` (`conflict_inspection.rs`) serialize `ViewId`, `ChangeId` and the
  other IDs with their derived `Serialize`, so `--format json` prints 32 numbers
  where the human output and every other document print the hex ID.
  `frontier list` had the same problem and now uses `command_output::display_id`.
- `[ ]` **Fetch pushes whole objects, even when the client has the old version
  (2026-09-26).** The push sends every object the walk finds in full. Some objects
  grow with the repository and change with every view: a content-set snapshot is
  about 3.3 MB per view at 100,000 files. Measured by the `fetch_incremental`
  benchmark: one change pushes 6 KiB after 10 changes but 44 KiB after 200. Send
  such objects as deltas against their aligned old counterpart, which the client
  has. Then add a bytes budget to `fetch_incremental`.
- `[ ]` **The size limits for one file disagree (2026-09-26).** `wheat add` refuses
  files over 16 MiB (`DEFAULT_MAX_INPUT_BYTES`), Git import takes up to 64 MiB
  (`MAX_IMPORT_FILE_BYTES`), materialization up to 256 MiB, and the store and protocol
  up to 1 GiB. Decide which limit a file should have.

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
- `[ ]` **Durable workspace and store files are JSON (2026-10-05).** Bart's rule:
  temporary files may stay as they are, anything durable is BinSchema, and files
  people are expected to read or edit outside the CLI get a written exception in the
  BinSchema-only rule (`CLAUDE.md`, `crates/wheat-core/CLAUDE.md`). Durable JSON
  today: `store.json` and `.wheat/workspace.json` (`workspace_metadata.rs`), the
  registry pins `workspaces/<id>.json` (`workspace_registry.rs`), `remote.json`
  (`remote_config.rs`), and the remote catalog head (item above). Temporary, so
  fine: `materialization.json` and `progress.log` (`materialization.rs`),
  `rewrite-transaction.json`. Human-edited, so an exception: `config.json`
  (`authoring_metadata.rs`). Decide per file whether anyone reads it outside the
  CLI (`remote.json` holds the URL), move the rest to BinSchema, and write the
  exceptions into the rule.
