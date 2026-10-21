# Current task

## 2026-10-21: Git push/pull round trip (@calm-lynx)

Bart: "start out building a test case for git based push/pull workflow so that we
know the happy path works and stays working ... It's fine if the test fails." His
choices: the command is `wheat pull <git-url>` (a Git remote recognised and imported
incrementally, then integrated); unpushed Wheat changes end up **on top of** the new
Git commits (like `git pull --rebase`), so the next push fast-forwards; and Wheat's
own pushed commits come back as the same changes (same store), not duplicates.

- **Done 2026-10-21**: `wheat pull <git-url>` is implemented and
  `crates/wheat-cli/tests/git_round_trip.rs` passes. Pieces: wheat-store
  `repository_rebase.rs` (036e0e3); wheat-git `pull.rs` and the projection's commit
  refs (`bridge.rs`); wheat-cli `git_pull.rs` and the URL dispatch; tests
  `git_pull.rs` (15) and `integrate.rs` `a_git_pull_waits_for_integrate`; bench case
  `git_pull_incremental` (results `bench/results/wheat-quick/20261021-033448-*`).
  Profiling found pull paying full validation twice; the label lookup no longer
  validates, and the remaining one (`frontier_service::list`, 2.7 of 3.2 s at 2,000
  commits) goes with incremental-validation step 9. Follow-ups are in `TODO.md`
  ("Found while dogfooding").
- B1 (push dropping changes before a reused anchor) fixed in 6d46522.
- Bart's decisions (2026-10-21): conflicts while reapplying are **durable
  conflicts**; merges are **first parent only** (one change per first-parent
  commit, edits vs first parent); commits this store pushed are found through new
  **reverse refs** written by push (commit-keyed, O(log P)); the **label stays**
  where it was imported and the workspace selects the new frontier; a rebased root's
  dependencies are **only the new tip**; rebased metadata is **like git rebase**
  (author kept, committer = puller, now); the branch is the **label's Git ref**
  (`original_git_ref`) with a `--from` override, refused for native labels without
  it; and pull **requires a single frontier** (integrate first). Mine, Bart may
  override: a rebased revision is authored at its old spot (one path, valid when it
  conflicts); a pushed commit's tree is re-hashed and compared as push does; a known
  commit off the workspace's path is refused.
- Plan (from a planning pass): wheat-store `repository_rebase.rs`
  (`append_and_reapply` via `compose_authoring`, with `PendingRevision` gaining an
  `authored: Option<(ViewId, TreeId)>`, plus `ensure_continuation_edge` so the old
  frontier stops being a leaf); wheat-git `pull.rs` (`prepare_pull`: lock, observe,
  fetch, bounded first-parent walk to a known commit, proposals built with
  `tree_diff::tree_edits`/`metadata`); wheat-cli `git_pull.rs` and URL dispatch in
  `Command::Pull`; bench case `git_pull_incremental`.
- Investigation findings (all in `TODO.md`, "Found while dogfooding"): import
  rebuilds a fresh state and is bound to a source snapshot; the Wheat→Git mapping
  is only the store's `projections/git.git` refs; imported changes depend on the
  first parent only; and push skips applications before a provenance-matched tip
  without a tree check (`export.rs:238/282`, the B1 bug), which an integrate-based
  pull would hit as soon as the native change sorts first.

## 2026-10-20: hash content once (@calm-lynx)

Bart, on the protocol follow-ups: "use publish key and batching small files is a free
win here. We should probably hash content only once if possible." Batching is done
(65bb956) and so is `publish_key` (3d15d42). For hash-once, Bart chose a **verified
type**: a `VerifiedContent` made where bytes come in (server upload, client fetch,
local capture), carried through stage, validation and store, none of which re-hash
it. Keep every hash that is the only guard (bytes read from disk or S3, delta
reconstruction, external snapshots).

Measured map (Explore agent, 2026-10-20): a server-published blob is hashed 5–6
times before it is durable, a fetched blob 4, a `wheat add` file about 7; every
packed read is hashed twice (`verify_logical`, then `catalog.rs:1423`); transition
validation (`repository_transition.rs:161`) reads and hashes each new blob only to
check it exists. `ContentItem` (`repository_store.rs:23`) already carries content
into every backend.

Plan, one commit per stage:
1. A content-hash counter and a contract that counts bytes hashed per published
   byte; record the baseline.
2. Write path: `VerifiedContent` in wheat-core; stage, `ContentItem`, packed
   `put_publication`/`content_records`/`validate_record`, loose and S3 puts take it.
3. Validation checks existence, not a full read.
4. Local capture (`edit_capture` → `ContentWriteBatch` → `put_many`).
5. Read side: the second hash on packed reads, and seal/repack re-hashing records a
   verified lookup just read.

Progress:
- Stages 1–3 are done (one commit): a published blob goes from 6× to 1× on a loose
  server. `VerifiedContent<B = Vec<u8>>` (owned or `&[u8]`); `ContentItem { content,
  base }` now lives in content_store.rs; `ContentBackend::put_many` and every
  `ContentStore` put take verified content; the packed catalog takes `CheckedRecord`
  (made by `check`, `content` or `object`); `ContentStore::contains` on a backend and
  step-5 validation use `logical_length` (metadata only). Contracts:
  `a_published_blob_is_hashed_once`, `verified_content_is_stored_without_hashing_it_again`.
- There's no end-to-end count for the client fetch: CLI tests run the client as a
  subprocess, so the in-process counter can't see it. The fetch mints one
  `VerifiedContent::check` per blob (remote.rs `stage_blob`), and the store contract
  covers the rest.
- Not benched yet. When I tried, @lucky-raven had an in-flight `structural_delta.rs`
  change (publish path), so the bench would have measured their code too. Run
  `just bench` once the tree is quiet and commit the results file.
- Stage 4 done: `CapturedEdit::content`, `ApplicationResult::generated_content` and
  `ContentIo::store_generated` carry `VerifiedContent`; capture hashes each canonical
  snapshot once (it hashed it twice, then `ContentWriteBatch::put` a third time);
  `ContentWriteBatch::put_verified` and `ContentStore::put_verified` (public now) store
  without hashing. The CLI, the Git importer (`edits_between`, `tree_diff`) and
  `repository_mutation` use them. Contracts: core `captured_content_is_hashed_once`,
  store `verified_content_is_staged_without_hashing_it_again`. Still hashed twice on
  `wheat add`, by design: the workspace scan hashes raw file bytes to compare with the
  index, and capture hashes the canonical (newline-normalized) bytes, which can differ.
- Stage 5, packed reads (Bart chose "verified envelopes"): a read hashed 3× from a
  sealed or active segment (reader, catalog again, `resolve_delta_chain`), 2× from the
  bulk tail, and staged/pending reads re-hashed verified blobs. Packed readers now return
  `VerifiedPayload` (made only by `verify_logical`/`verify_payload`); the catalog no
  longer re-verifies; `ContentBackend::get_stored` returns `StoredContent::{Verified,
  Envelope}` and `resolve_delta_chain` hashes only envelopes. Seal rebuild and metadata
  repack write the read payload as a `CheckedRecord` without re-checking it, and
  `prepare_delta_record` takes a `CheckedRecord`. Contracts:
  `a_packed_content_read_hashes_its_bytes_once`,
  `content_reads_beside_a_content_generation_hash_once`,
  `staged_content_reads_back_without_hashing`.
- Stage 5, resolve: `ContentIo::resolve` and `apply_edits`'s resolver return
  `VerifiedContent`; `apply_edits` compares ids instead of re-hashing, so it hashes only
  the content it generates. `resolve_delta_chain` returns `VerifiedContent`, exposed as
  `ContentStore::get_verified` (used by the store `ContentIo`, repository verify, the Git
  importer and `wheat move` replay). Contract: core
  `application_hashes_only_generated_content`. Still hashed per call: `FileBytes::value`
  in `edit_move.rs`.
- Benched at e0705d4 (`20261020-040238-e0705d419805.tsv`, all 16 cases, none failed).
  No full-suite run exists from before the hash-once work; the only cases shared with
  65bb956 (head_receipts, publish_contention, remote_latency) didn't regress, and
  1–2-writer server CPU per publish fell ~20%. Bart then allowed a `git init` in a
  scratch export, so 65bb956 got a full run plus repeats (results not committed: the
  scratch repo's rev stamp is fake). With 2–3 runs per tree (65bb956 / 01013f1 / HEAD):
  publish_content add 1.8–2.0 s → 1.3–1.9 s, publish 1.8–2.2 s → 1.4–1.8 s, add_scale
  add_modified 4.3 s → 3.6 s, deep_stack moving 1.03 s → 0.86 s. Clone wall and
  packed_append overlap across trees (noise). Clone server peak RSS was 64–119 MB at
  65bb956 and a steady ~64 MB since. fetch_incremental's +13% objects / +26% reads per
  change come from e0705d4 (fetch-walk fix); at 01013f1 they matched 65bb956.
- Objects (not content) are still validated twice on remote_packed seal (at
  `put_if_absent` and again via `CheckedRecord::check` in `seal`). That's out of
  scope; it's noted here.

## 2026-10-09: integration (pull) — Bart: "clean up those small items. Then pick up
integration, since nobody can use the thing without it."

### Small items: done

- 71b5f70 content fetch ends with an Error for an unreadable blob (was a hang).
- d8a0c41 websocket message limit 16 MiB on both ends (frame limit already was).
- d8b293a publish seals objects before the head moves (was after: S3 head could
  name objects never stored); seal error no longer leaks its message.
- a563fb1 a directory and a file can trade places in one revision (reader,
  edit application via `apply_final_edits`, capture settlement, lookups).
- d69d379 `wheat ignore import`.
- New TODO: `status` lists an untracked directory and each file in it.

### Integration: Bart's decisions (2026-10-09)

- Fetch with unpublished work: **union** the remote state into the local one.
- Labels on fetch: **three-way** against the last fetched manifest; a label both
  sides moved differently makes fetch refuse and list them.
- UX: **`wheat integrate`** (join the active label's frontiers, or chosen ones,
  into one view and materialize it) **and `wheat pull`** (fetch + integrate).
- Divergent revisions of one change in the joined frontiers: "Order them by date
  and see if the changes in the revisions still apply on top of one another? At
  worst we get the same changes in a new revision." Reading: earlier revision
  (by date) + the later one's net edit changes since their common predecessor
  → a new revision with the earlier as predecessor. Open: what to do when that
  merged revision does not apply cleanly (authored revisions must be clean) —
  default plan: refuse, naming the change and paths; ask Bart if it comes up.

### Facts that shape it (from research)

- State = base_view + change map + lineage map + content manifest + 4 retained
  sets, all grow-only. Labels live in the envelope, outside the state.
- Publish validation (`repository_transition.rs:45-206`): same base_view, retained
  sets only grow, old lineage edges kept, change map entries added/changed only to
  retained objects, content only grows. Labels unchecked; publish sends the local
  manifest wholesale.
- Mutations never move labels; new work shows as descendant frontiers (leaf views
  of the lineage below the label's view). Integrate = join frontiers
  (`compose_changes` in the design docs): reuse revisions, new applications/views,
  durable conflicts, continuation edges from each joined frontier.
- Retained changes must not hold two change objects for one ChangeId
  ("duplicate change identity"), so the union refuses a ChangeId whose change
  object differs between the sides.

### Step 1 notes

- `crates/wheat-store/src/repository_union.rs`: `RepositoryReader::union_states`,
  `union_envelopes(base, local, remote)`, `merge_labels`, `RepositoryUnionError`.
- `radix::union` is now paired_diff + update_batch (was: read both whole; its doc
  comment claimed otherwise). `ChangeMapStore::union` (DivergentEntries error),
  `ViewLineageMapStore::union` (child-set union per shared parent).
- `native_fetch` keeps local work (`kept_local` in JSON); `--force` takes remote.
- Tests: `crates/wheat-store/tests/repository_union.rs` (union == both histories
  recorded in one repo; read count 41 vs 55 for 16 vs 160 shared changes),
  `crates/wheat-cli/tests/remote_fetch.rs` (3 new).
- `frontier list --format json` printed IDs as byte arrays; fixed (helpers moved to
  `command_output`). status/conflicts have the same bug: TODO.md.

### Plan: all four steps committed

1. Union fetch + three-way labels — 0631439.
2. `wheat integrate` — 19c47af (`repository_integration.rs`; canonical order,
   durable conflicts, continuation edges; CLI `integrate_workspace`).
3. Divergent revisions per Bart's rule — c311917. A combination that does not
   apply cleanly is refused (`RevisionsDoNotCombine`), naming the change.
4. `wheat pull` — dfdd85f (fetch + integrate; one frontier → switch as refresh;
   a refused integrate keeps the fetch).

Scale is covered by read-count tests in `crates/wheat-store/tests/repository_union.rs`
and `repository_integration.rs`, not by a wheat-bench case. Full suite at dfdd85f:
1,121 passed, 18 skipped.

Follow-ups in TODO.md: status/conflicts JSON IDs as byte arrays; combined revisions
carry no subsumption evidence. Steps 1-4 and the site were pushed 2026-10-10.

### 2026-10-15: the FrontierRevisesBasis refusal (Bart: "important to do right now")

Committed 3588883 (radix `cursor_from` seek) and 5a4fdb6 (integrate starts below the
lowest revised/out-of-order label change; resolve on a label-held conflict now
stays a frontier; `prefix_view(0)` fix). Details in TODO_DONE.md under "Loose ends".
Not pushed. Full suite 1,127 passed, 18 skipped.

## 2026-10-15: label moves — design (Bart: "lets think about how to do label moves")

Plan: `docs/design/wheat-label-moves.md` (untracked), with Bart's decisions 1-11.
Codex reviewed it twice (thread 01a13dbf-fb44-7ba3-b181-d10e6ec464ed). Round 2 found
the ancestry approach (`combines` on revisions as extra predecessors) still leaky:
`Joined::Existing` reuse records no ancestry; criss-cross merge bases; the BFS stop
condition doesn't find nearest common ancestors; squash erases target-side removals
and `combines`; publish validation must use equality (`pred.edits == preserved ∪
removed`) and can't trust claimed merge parents; `combines` missing from object_graph,
structural_delta, rewrite retention, reconstruction; displaced materialization vs the
journal's basis invariant; squash boundary uses the remote, not the basis; fetch rule
for label create/delete/metadata; resolver and materialization still fully validate.
Existing bugs found: `join_revisions` resurrects removed edits (J/B2 example in the
plan); publish doesn't check new revisions against their predecessors.
Bart chose the ancestry DAG; review 3 fixes are in the plan (pairwise merge,
squash never collapses combined revisions, decision 12 per-edit removal evidence,
journal/materialize changes). Decision 13: incremental validation at publish/fetch is
a prerequisite project: `docs/design/wheat-incremental-validation.md` (untracked).
Codex review 1 of that plan (same thread) found: union of two valid increments can
make a lineage cycle; forced rewrites need full validation (or checks of surviving
references); step 5 must check revision.change_id == key and dependency closure;
integrate can add a non-composition edge into an existing view; full validation's
"exactly one child" rule is ambiguous with shortcut edges; change-map bijection;
bootstrap/base mode; content membership must be against the aggregate manifest;
`last_fetched` is set from the server's publish ack without checking it equals the
offered envelope; content manifests are flat snapshots, so content checks are
repository-sized; closure/parent-discovery/budget algorithms unspecified; canonical
order must be validated (criterion: every change of T after k's last dependency in
T's canonical order has a lower id than k).
Bart chose: canonical order validated via an application position + binary-lifting
max-ID index; content manifests become radix maps (ContentId → count) now.
Validation plan reviewed 5 rounds; round 5 found no new P1/P2. Key additions: view
`size`/`last`/`weight` with weight(child) > weight(parent) on every lineage edge
(acyclic per edge); revision `combines` (≤64) + `generation` and always-authored
combined revisions moved into the prerequisite; canonical radix encoding (I9) checked
in validators; orphaned applications refused; publish ack must equal the offered
envelope. Label-moves plan aligned to build on it.
Both plans committed in fcf8483. Bart: "lets start with plan 1".

### Plan 1 implementation (docs/design/wheat-incremental-validation.md, "Order of work")

- Step 1 done, 41c54f7. Bart chose to fix the updater rather than check I9 only in
  validators: `update_node` returns `Updated::{Node, Leaf, Pruned}`, and
  `validate_node_at` refuses a value on a non-root node on every read. Parity
  proptests are in `crates/wheat-store/tests/collection_builder_parity.rs`. Suite:
  1,138 passed, 18 skipped.
- 7551122: `update_batch` was per-edit (a root rewrite per key; the batched path
  mishandled prefix splits). Now one pass. Gothab replay as radix content set: 377 MiB
  → 32 MiB loose (flat snapshots 218 MiB). This answers why 651c01b left radix.
  Recorded in the plan's "Content maps" section. I asked Bart whether to
  measure first or go ahead; there was no answer, so I measured, and the numbers
  support his radix decision.
- Step 2 (content maps), split in two:
  - 2a: add `ContentSetNode` (radix set, tag 0x0c01, segment/loose code 12) and
    `ContentMapNode` (ContentId → u32 count) with stores, builders, every
    registration (object_graph, structural_delta, store_analyze, metadata_repack
    pairing, segment codes, loose ObjectKind) and parity tests.
  - 2b: ViewV1.required_content → ContentMapId, state → radix ContentSetId; drop
    ContentManifestV1/snapshot/delta/chain code; migrate users (inventory: the
    Explore agent's report — commit_content, commit_writer, repository_mutation,
    union, transition, rewrite, hydration, verify, verification_scratch,
    object_graph, structural_delta, metadata_repack, store_analyze, remote.rs
    fetch_content, server publish, wheat-git finalize + content_accumulator +
    staged_import verify_mapping, core model/repository/root_builders/wire).
  - Possible parity bug to check in 2b: `repository_rewrite.rs:562-645` rebuilds the
    aggregate without application/conflict partial trees and conflict evidence
    targets, which core `canonical_closure` includes.
  - 2b done, uncommitted (2026-10-15). The rewrite now adds application result trees
    and conflict evidence targets to the aggregate. No test fails without that fix:
    every retained application's target view is retained, and its produced view is
    usually another application's target, so I found no scenario reaching the gap.
    `rewrite_preserves_a_preexisting_conflicted_descendant_application` now runs
    `validate_complete`. Verifier: base map against its tree in full, every other
    view's map against its first lineage parent's by the tree diff, full comparison
    on a diff limit (unit tests `repository_verify::content_map_tests`). Import resume
    authenticates content-map nodes once with position-keyed receipts
    (`authenticate_content_map_once`); the finalize contract
    `finalization_reads_each_views_content_change_not_its_whole_map` fails at 3,448
    reads vs a budget of 310 if views are diffed against the empty map. Suite: 1,140
    passed, 18 skipped; clippy, fmt, check-windows clean.
  - 2b committed as fece015 (Bart: "Commit it whole with mine", so publish.rs went in
    whole, including @calm-lynx's span hunks).
  - The publish_content bench fails `clone.peak_rss` (430–513 MB against 256 MiB). Not
    step 2. Massif on the clone puts 384 MiB of the 397 MiB heap peak in
    `materialization::load_install` → `ContentStore::get`, under `Batcher::next_batch`:
    since a88d542 (2026-10-05, batched materialization) a batch holds up to
    `MAX_OPERATION_RESIDENT_BYTES` = 512 MiB of file content, so all 32 × 12 MiB
    blobs are resident at once. The budget was measured 2026-09-26, before a88d542, and
    no publish_content run between then and 2026-10-15 recorded the clone's peak.
    Bart chose a separate batch cap: `MAX_BATCH_RESIDENT_BYTES` = 64 MiB (the clone
    peaks at 98 MiB), with unit test `batches_of_large_files_hold_at_most_the_batch_bound`.
    Pushed through fece015 on Bart's OK. Repro: wrap `target/release/wheat` with
    `valgrind --tool=massif` for `clone … reader` and point `WHEAT_BENCH_WHEAT` at the
    wrapper.
- Step 3 done (2026-10-16): revisions carry `combines` (≤ 64, never the predecessor)
  and `generation`; views carry `order { size, last, weight }`. `join_revisions`
  always authors a combined revision (earliest input as predecessor) and refuses
  more than 65 lines (`TooManyRevisions`). Combined revisions replay like reapplied
  ones: `Replay::{Author, AuthorCombined, Apply}` in `repository_mutation.rs`.
  Full validation checks generations, combines inputs (retained, same change), each
  view's order against its composition parent (`verify_view_order`) and the weight
  rule on every lineage edge. Tests: `crates/wheat-store/tests/view_order_validation.rs`
  (forgeries), new cases in `tests/repository_integration.rs`, core
  `revision_parents_and_view_order_are_checked_on_decode`, object_graph combines refs.
  Suite: 1,153 passed, 18 skipped. Release bench (transition_validate, deep_stack,
  checkpoint_stack) shows no regression. Committed as c3157c5 (not pushed).
- Step 4 done (2026-10-16): applications carry `ApplicationOrder { position, previous,
  skip, window_max }` (core `model.rs`, with the `begin`/`wanted`/`extend` builder and
  the `RangeMax` query, both cursors so sync core and async store share them). Store:
  `RepositoryReader::{application_order, highest_change_from}`; `apply_step` and the
  commit writer fill the index; full validation recomputes it per retained application
  and checks I2 (`verify_application_order`). Nothing existing needed reordering: every
  producer already composes in canonical order. Tests: core `tests/application_order.rs`
  (brute force at positions 0–1,000, reads = set bits), store
  `view_order_validation.rs` (index forgeries, out-of-order view refused, an exact
  control, a low ID after its dependency accepted), object_graph skip refs. Suite:
  1,161 passed, 18 skipped. Bench: no change beyond noise (deep_stack.reapply 0.91 s
  under load, 0.77 s rerun). Next: step 5, full validation tightened.

## 2026-10-15: publish throughput (@calm-lynx) — Bart: "We should be able to do 10 of these"

Plan and numbers: `docs/design/wheat-lessons-from-scion.md` ("Measured contention",
Outstanding). Done and committed: staged writes + TCP_NODELAY (0446f8b), server-side
join of publishes behind the remote (79bbb99). `publish_contention` now needs 1 attempt
per publish at 1-16 writers but tops out at ~8.6/s at 8 writers (target 10): publishes
queue on `RepositoryEntry::commit`, about 115 ms each at 8 writers, server ~4% busy.

Bart: "Always profile first". Done: wheat-trace crate (415cd40), publish spans (in
fece015), packed append step spans (c0ae347), profile written up in the design doc
(3b8010a, "Profile at 8 writers"). The bottleneck is the packed store: two ~46 ms
appends per publish, and one catalog mutex every read waits on during appends.

Repro: `WHEAT_TRACE=/tmp/x.json target/release/wheat-bench quick --case
publish_contention --no-save --output /tmp/p.json`; analysis scripts that split the
trace by writer level were in /tmp/wheat-profile/scripts (levels.py, wait.py,
append.py; levels split on >1.5 s gaps between publishes, so 1/2/4 writers merge).

Bart: try all five options cumulatively on main, measuring after each (1. catalog
lock outside the append's fsyncs, 2. one append per publish, 3. cheaper append,
4. group commit, 5. blocking work off the runtime).

Option 1 done 2026-10-16: `SharedCatalog` in packed/catalog.rs, `AppendJob` runs the
I/O without the catalog mutex, locator in WAL. Test
`reads_proceed_beside_an_append_and_see_only_published_records`. 8 writers 9.2 ->
13.5/s; 2 writers 6.7 -> 5.7 is noise (earlier runs 5.05-6.72; the server is idle
~75 ms between publishes at 1-2 writers, so clients set the pace; per-level split
script /tmp/wheat-profile/scripts/bylevel.py). Appends (33-37 ms,
two per publish) now run 96% of the time. Numbers in the design doc ("Reads beside
an append"); trace in /tmp/wheat-profile/option1.json.

Option 2 (one append per publish), Bart's choices 2026-10-16: stage content too (a
refused publish leaves nothing behind); a new wheat-store trait `RepositoryStore:
ImmutableObjectStore` with `content_store()` and a joint objects+content write,
implemented by PackedObjectStore (one put_records) and RemotePackedStore;
RepositoryEntry holds one `Box<dyn RepositoryStore>`. Applies to the server publish
AND the client fetch. Validation reads added content in full
(repository_transition.rs:161), so staged content must be readable before it is
stored, like BatchObjectStore's staged objects.

Option 2 done 2026-10-16: `RepositoryStore` / `RepositoryStage` /
`SeparateRepositoryStore` in wheat-store/src/repository_store.rs; server publish and
client fetch/clone/publish-join stage content and objects and flush once. 8 writers
13.5 -> 18.8/s. Now one 38 ms append per publish inside the commit lock (lock held
~52 ms: append 38, head CAS 10, rest 3), so the ceiling is ~19/s. Trace:
/tmp/wheat-profile/option2.json. Found: delta content on packed goes loose with its
own fsyncs (TODO.md).

Option 3 (cheaper append), Bart's choices 2026-10-16: append-only with no journal,
the head inside the segment, locator `synchronous=NORMAL`. A V1 segment cannot grow
append-only (its payloads must be contiguous, and a cumulative tail per append is
quadratic in space), and sealing already rebuilds the records through
SegmentBuilder, so the active segment became a log: new BinSchema types
`ActiveLog{Header,FrameFields,FrameHeader,FrameTrailer}V1` (wheat-objects schema,
wrappers in wheat-core segment_records.rs), packed/active.rs rewritten. Each append
writes one frame (header with a digest of its fields, payloads, an index page, a
trailer with a chain digest) and syncs once; open walks the frames and the newest
verified one is the state. `active.head`, `active.append` and their hand-rolled
encodings are gone. Bart did not answer the two format questions; defaults taken:
open still hashes the whole log (as before), and a damaged frame with a later frame
after it refuses to open (only a crash-torn tail is truncated). Analysis tools open
the log with `packed_segment_reader::open_active_segment`. Locator schema v6.

Option 3 done 2026-10-16: append 38 -> ~4.4 ms (segment write + one sync 3.7, locator
0.7). Trace: /tmp/wheat-profile/option3.json (tracing distorts 16 writers: 333k
`packed_catalog_wait` spans). The head CAS (publish_cas, 7-9 ms, atomic head-file
write) is now the largest step under the commit lock. Found and put in TODO.md: a
crash between a seal's publication and the active segment's retirement duplicates
its records (the old head check never matched); catalog.head and repack.intent are
hand-encoded.

Option 3 committed 45a9cc2. Quiet runs at 8 writers: 40.2, 45.2, 43.4 accepted/s.
Lock hold per publish at 8 writers (option3.json): CAS 9 (temp file sync + directory
sync), store 5.5 (one sync), validate 1.6, join 0.9.

Option 4 (group commit) done 2026-10-16. Bart did not answer the shape question;
default taken: full batch. Design (written up in the design doc, "Group commit"):
- wheat-store: `RepositoryStage::take` -> `StagedPublication`; `PendingPublications`
  (stores several in one `put_publication`; `over(stage)` gives a `PendingView` that
  reads them beneath a stage). Only the commit-phase join and validation use the view:
  the receive phase must NOT see batched objects/content as present, or a failed
  batch would leave a later publish naming data never stored.
- wheat-server `commit.rs`: `commit::accept` joins/validates under the state lock
  against the tentative head, moves the stage into the next batch; a spawned
  committer stores, seals, and CASes per batch, re-joining when the durable head is
  not the batch's base. A failed batch fails the next one (stale). A panicking
  committer is cleaned up by the `Abandoned` guard. Tests:
  `wheat-server/tests/group_commit_test.rs` (gated store), `repository_stage.rs`.
- Result: 16 writers 50 -> 63-70/s; 8 writers unchanged (35-42; the committer is busy
  ~50%, the writers' cycle outside the server's publish handling, ~170 ms, sets the
  pace). Trace /tmp/wheat-profile/option4.json.

Option 5 done 2026-10-16: only the committer moved to the blocking pool
(`spawn_blocking` + `Handle::block_on`); other disk work is short reads, and the
server is ~5% busy at 8 writers, so per-read handoffs would cost more than they save
(TODO item "wheat-server does blocking disk work" updated). Gains within noise (8
writers 41-47/s, latency p50 83 -> 76 ms).

Not yet profiled: from 2 writers on every publish is joined and the median publish
latency jumps 18 -> ~55 ms; the server adds ~1 ms, so it is likely the client
taking the joined head.

Contract rules done 2026-10-16 (`publish_contention.rs`: writers_8.accepted_per_second
>= 10; attempts_per_publish and attempts_max <= 1 at every writer count; new
`Recorder::at_least`). Full-size run: no violations.

Left of Bart's plan: "maybe" the client-side push of the difference (design doc
Outstanding item). Not started; needs Bart's go-ahead.

## 2026-10-17: hand-rolled persisted encodings (@calm-lynx) — Bart: convert all, strict guard

How they got in: all predate the BinSchema-only rule (first in
`crates/wheat-core/CLAUDE.md` 2026-09-24, `7f5420c`; root CLAUDE.md 2026-09-25,
`6ce2538`); nobody swept the existing code. Bart picked "convert all, strict guard".

To convert (each gets a BinSchema record + wheat-core wrapper, call sites switched):
1. `catalog.head`, `repack.intent` (wheat-store packed/catalog.rs), plus the hand
   reader of catalog.head in wheat-cli store_analyze.rs (and its test fixture).
2. `repository.head` WHEATHEAD (wheat-store publication.rs), plus the hand reader in
   wheat-cli store_analyze/semantic.rs.
3. Content envelope header WHCE (wheat-store codec.rs). BinSchema layout can be
   byte-identical (uint32 magic, uint8 version, uint8 codec, two uint64).
4. Snapshot manifest WHEATSNP (wheat-store snapshot.rs; decoded, bounded by limits).
5. Ownership manifest WHEATOWN (wheat-store ownership.rs; only hashed into its ID).
6. Materialization progress.log WMI1 (wheat-cli materialization.rs; fixed 36-byte
   records with crc32, torn-tail handling).
7. Import source snapshot (wheat-git staged_import.rs `snapshot_bytes`; stored as a
   blob in the import journal and compared byte-for-byte, never decoded).
8. Digest inputs hashed by hand: content_generation.rs `index_digest` and
   `fingerprint_entries` (both end up persisted in the manifest).

Not storage, no change: `guarded/matcher.rs` word compare (in memory); integer ->
ID helpers in tests; wheat-bench synthetic data; `import.marker` (a constant marker
in a temporary staging directory).

Items 1-7 done: schema types `PhysicalCatalog{Ref,Head}V1`, `RepackIntent{Fields,}V1`
(wrappers in segment_records.rs), `RepositoryHeadV1`, `ContentEnvelopeHeaderV1`
(byte-identical to the old WHCE header), `InventoryEntryV1`, `SnapshotManifestV1`,
`OwnershipManifestV1`, `MaterializationProgress{Fields,Record}V1` (crc32 stays in
the CLI), `GitImport{Source,Ref}V1` (wrappers in the new wheat-core
store_records.rs). Tests: wheat-core tests/segment_records.rs, tests/store_records.rs.
Suite 1183 passed. Found: the generated encoder writes a uint32 count as
`len() as u32` (silent truncation); the new wrappers refuse longer arrays.

Bart's answers 2026-10-17: count checks -> "Fix it in binschema" (cap the decode
reservation at bytes left / the item's minimum encoded size, re-pin, regenerate, drop
the hand count reads in segment_records.rs, content_generation_wire.rs,
store_records.rs `inventory_count`). Digest inputs (item 8) -> "Hash BinSchema
bytes" (each entry's ContentGenerationEntryV1 encoding; a small schema type for the
index digest input).

Guard: `crates/wheat-core/tests/no_hand_rolled_encodings.rs` (syn; skips generated/,
tests/, `#[cfg(test)]`, wheat-bench, wheat-trace) fails on `to_/from_{be,le,ne}_bytes`
and `byteorder`. A byte-string-const rule was left out: it would flag hex tables and
hash domain tags. Exceptions: `NOT_PERSISTED` (matcher.rs only); a stale entry fails.

Done 2026-10-17. binschema `9af4a10` adds `capacity_hint_sized` and has the
generator emit it with each item's minimum encoded size. Both wheat gen scripts are
pinned to it; `wheat-gen-schema` now also re-exports `BitStreamEncoder`. The
hand count reads are gone: a forged count now fails as a decode error (out of input),
and limits (`MAX_PHYSICAL_CATALOG_SEGMENTS`, shard entries, snapshot max_objects/
max_contents) are checked after decoding. Item 8: `ContentGenerationIndexDigestInputV1`
plus `content_generation_index_digest` / `content_generation_fingerprint` in
content_generation_wire.rs; the fingerprint reuses one wire entry and one encoder, and
no longer hashes the entry count, since each entry's encoding delimits itself. Guard passes.
Suite 1186 passed; clippy, fmt, gen-schema checks, check-windows clean;
`sharded_manifest` and `content_repack_plan` in release are within noise of
2026-09-25. Still open in binschema TODO.md: the encoder's `len() as u32` truncation.
