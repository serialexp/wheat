# TODO — done

Finished items, moved here from `TODO.md` under the same headings.

## Security and performance scan (2026-09-26)

- `[x]` **wheat-server has no authentication or access control (confirmed).**
  `run_session` (`crates/wheat-server/src/connection.rs`) never looked at
  `ClientHello.auth_token`, so any client that could connect could open, fetch,
  publish or force-publish any repository. Done 2026-09-26: `AccessProvider`
  (`crates/wheat-server/src/access.rs`) checks the token before `SessionEstablished`
  and `read` / `publish` / `force` access before every operation; a repository
  without access answers like a missing one. The standalone server reads token
  hashes from the `[auth]` table of `--config wheat-server.toml`
  (`wheat-server hash-token` makes them), runs open only on loopback, and refuses
  browser upgrades whose `Origin` is not allowed. The client gets its token from
  `credential.helper` (`crates/wheat-cli/src/credential.rs`). Tests:
  `crates/wheat-server/tests/access_test.rs`, `cli_test.rs`,
  `crates/wheat-cli/tests/remote_auth.rs`. TLS is still open (see "Loose ends").
- `[x]` **Materialization does not reserve `.git` (confirmed).** A repository could
  carry `.git/hooks/...` or `.git/config` and plant something git runs later if the
  workspace is also a git checkout. Done 2026-09-26, as a question rather than a
  ban, because such files can be legitimate: before writing anything,
  materialize, switch, refresh, restore, clone and import list every path with a
  `.git` component (any letter case, any depth). A person at a terminal is asked;
  anything else is refused with `dot_git_paths` (exit 2) unless `--allow-dot-git`
  is given (`crates/wheat-cli/src/dot_git.rs`). A refused clone or import leaves the
  workspace set up and says to run `wheat materialize --allow-dot-git`. The three
  reserved-name lists are now one, `wheat_ignore::WORKSPACE_CONTROL_NAMES`. Tests:
  `crates/wheat-cli/tests/dot_git.rs`. Capture is still open (see "Loose ends").
- `[x]` **Generated decoders trust untrusted lengths (confirmed).** The
  BinSchema-generated decoders reserved `Vec::with_capacity(length)` straight from a
  length read from the input; six bytes (a `TreeV1` tag and a `u32::MAX` entry
  count) asked for hundreds of GiB, and a failed allocation aborts the process.
  Done 2026-09-26 in BinSchema `7377cb9`: every input-sized reservation goes
  through `BitStreamDecoder::capacity_hint`, capped at the bytes left, and byte
  arrays decode with one bounds-checked `read_bytes_vec`. The pin had said
  `f48a34a` while the committed output came from `b99d34b` (a later commit on
  `main`); it now names the revision that produced the output. Tests:
  `crates/wheat-core/tests/forged_lengths.rs`,
  `crates/wheat-protocol/tests/forged_lengths.rs`. DeltaZstd and chunk limits are
  still open under the same heading in `TODO.md`.

## Revise/compose cost at stack depth (2026-09-25)

- `[x]` **`open` walks every view since the basis on each mutation (2026-09-26).**
  `PersistedRepositoryMutator::open` checked the effective view with
  `ViewLineageMapStore::is_descendant(basis, effective)`, a forward search over every
  view created since the basis, so each `change new`/`add` was linear in the history
  and building one was quadratic (4,000 revisions took 283 s). Done 2026-09-26: the
  effective view's applications name the view each change was applied onto; in
  canonical order they are the path from the basis, and `ViewLineageMapStore::follows_path`
  checks each hop is a lineage edge, one lookup per change. The forward search stays
  as a fallback. The `checkpoint_stack` bench's spread top revision went from 86 ms to
  about 6.5 ms. Tests: `proving_ancestry_does_not_grow_with_the_history_under_the_basis`
  and `a_mutation_refuses_an_effective_view_outside_the_basis_lineage`
  (`crates/wheat-store/tests/repository_mutation.rs`),
  `a_path_proves_descent_hop_by_hop_without_scanning_the_lineage`
  (`crates/wheat-store/tests/view_lineage.rs`).

## Explicit history collapse, repack, and GC

- `[x]` **`wheat change squash`.** Collapse one change's unpublished checkpoint
  suffix, retain the last published predecessor boundary, and rebuild affected
  descendants while preserving stable `ChangeId`s, latest canonical `EditId` sets,
  the final tree/partial tree, and unresolved-conflict meaning.
- `[x]` **`wheat stack squash`.** Apply the same normalization to every unpublished
  logical change in the selected stack, retaining one revision/application/view
  boundary per logical change rather than flattening the stack.
- `[x]` **Squash rewrite recovery atop the workspace registry.** Store-side pins now
  enumerate every attached workspace and conservatively cover materialization and
  repository-CAS transitions. The store-scoped rewrite transaction publishes squash
  plus affected workspace selections recoverably while retaining roots selected by
  other owners; ordinary head-dependent commands recover under the same admission
  lock before they can publish another repository head.
- `[x]` **`wheat repack` (nondestructive).** Rebuild local packed structural segments,
  indexes, compression, and physical content generations with reverse deltas while
  preserving repository authority, retained history, logical identities, unrelated
  authenticated inventory, workspace state, and observable semantics. Planning is
  read-only and capacity-gated; one authenticated physical-head commit selects the
  complete replacement, interrupted publication recovers old-or-new, and a canonical
  second run is a true no-op.
- `[x]` **`wheat repack` peak RSS is ~4.8 GB for the Gothab repo.** Measured
  2026-09-24 on a fresh disposable import of the 64.8 MB Git mirror (1,787
  commits, 208,190 content IDs): 177 s, peak RSS 4,848,536 KiB — the same as the
  older run (4,838,632 KiB). Import peaks at ~458 MB, so repack alone is ~10x.
  Fixed 2026-09-24: planning hydrated every view's full tree (~4.7 GB, ~146 s).
  It now reads the stored retained-view set and view-lineage map and pairs
  metadata and file content in one walk that only visits changed directories.
  `wheat repack` on Gothab is now 48.5 s with peak RSS 447,348 KiB, the store
  is slightly smaller (204.9 MB vs 205.6 MB), and the same state validates.
  See "Planning from stored records" in
  `docs/design/wheat-storage-efficiency.md`.
- `[x]` **Composed squash provenance for complex edit lineage.** Routine squash now
  preserves transition evidence when it can be represented exactly at the retained
  boundary and fails closed otherwise. Add an explicit composed-evidence representation
  before allowing collapse of checkpoint chains whose subsumption/resolution records
  refer to intermediate edits or applications that cannot survive verbatim.
  Done 2026-09-26: squash composes subsumption chains through the suffix and keeps
  only resolutions of conflicts still present at the boundary (with the view their
  application produced). See "Rewrite semantics" in `docs/design/wheat-local-cli.md`.

## Authoring workflow follow-ups (2026-09-24)

Found while fixing `add` not settling the materialized pin (captured paths showed
as untracked and `materialize` refused them).

- `[x]` **A change cannot be revised after an ancestor revision reapplies it.**
  `wheat change new -m A` → `add f` → `change new -m D --path other --value x` →
  `add --change A f` (reapplies D) → any `add` to D fails with `predecessor
  authored base does not match its current application target`
  (`workspace.rs` `author_from_workspace`). The reapplied application's
  `target_view` moves but the revision's `authored_against` does not, and the
  check treats that as corruption. This blocks the common flow of fixing an
  ancestor and continuing work on the change above it. Fixed 2026-09-24:
  `PersistedRepositoryMutator::revise` now authors every new revision against
  the view its predecessor is currently applied to (the view the workspace
  shows), as conflict resolution already did, so
  `apply(edits, authored_against) == resulting_tree` holds after a reapply.
  The CLI check is gone. Regression test:
  `crates/wheat-cli/tests/revise_after_reapply.rs`.
- `[x]` **`add` of 4,000 small files takes ~26 s wall for ~1.2 s CPU.** Almost
  all of it is I/O wait, which suggests per-object durable syncs. 4,097 paths
  also fail with `TooManyEdits { limit: 4096 }`, so a large initial import
  needs several adds. Profile, and consider batching syncs. Fixed 2026-09-24:
  - The 4,096 limit was per revision. It is now per file, with a
    revision-wide default of 250,000 edits
    (`DEFAULT_MAX_REVISION_EDITS`), enough for the largest aws-sdk-js-v3 and
    TypeScript commits (137,525 and 194,543 files).
  - Content goes through a `ContentWriteBatch`: about 40,000 syncs became 133
    for 5,000 files.
  - Every per-path tree lookup on the `add` and `validate` paths (workspace
    capture, settlement, mutation, commit writer, verify replay) now uses one
    sorted traversal (`MerkleTreeStore::lookup_many` / `path_shapes`). Before,
    each lookup decoded the shared directory again, which is quadratic in a
    wide directory.
  - `validate` resolves a view's content set once instead of once per file.
  - `init` now makes packed stores, and the loose store layout is removed from
    the CLI and the server.
  - Debug build, one directory: 5,000 new files take 1.0 s (was 64 s),
    5,000 modified files take 1.8 s (was about 200 s), and `validate` takes
    1.2 s (was 9 s). At 20,000 files they take 5 s, 11 s and 5 s (`validate` was
    127 s).
  - Tests: `crates/wheat-cli/tests/add_scale.rs` (5,000 files with a per-step
    time budget; an ignored 20,000-file benchmark) and
    `lookup_many_matches_lookup_and_reads_each_directory_once`.
- `[x]` **`plan_delta` loads every target file's full content only to check its
  size** (`materialization.rs`); the content is loaded again when installing.
  Fixed 2026-09-24: `ContentStore::logical_length` returns a file's length from
  stored metadata without reading its payload or resolving a delta chain. It
  uses the content generation's shard manifest, the packed catalog's index
  entry (a length-only lookup mode through the same locator cross-check as
  reads, covering the bulk tail and active and sealed segments), a loose
  envelope's 22-byte header, or a ranged header read on the remote store.
  `plan_delta` and `validate_materializable` size files with it; only symlink
  targets (at most 4,095 bytes) are still read, to reject NUL bytes.
  Installing still enforces the per-file limit on the real bytes. Tests:
  `logical_length_covers_every_packed_location_without_reading_payloads`,
  `logical_length_of_a_delta_reads_only_its_header`,
  `header_fetch_reads_the_length_and_checks_the_object_size`,
  `materializable_length_sizes_files_without_reading_them`, and a generation
  check in `generation_only_content_survives_a_second_repack`.
- `[x]` **`change new --path EXISTING --value …` without `--expected` fails with
  `authored edits do not apply cleanly`.** The message should say that an
  existing path needs `--expected`. Fixed 2026-09-24:
  `RepositoryError::InvalidEdits` now carries a `RejectedEdit` (path, the file
  found there, and the `EditFailure` reason) for each edit that failed, in both
  wheat-core and the persisted mutator. The message lists the first five.
  `change new` adds how to fix it: the path already exists (pass `--expected`),
  does not exist (omit `--expected`), or does not match `--expected`. Also
  fixed: model and store errors wrapped in a transparent
  `RepositoryMutationError` were hidden from the CLI's error classification, so
  they came out as `unexpected_failure` with exit 1. They now keep their class,
  and rejected edits are `edits_do_not_apply` with exit 2. Tests:
  `initial_edit_that_does_not_apply_explains_expected`,
  `model_errors_inside_a_mutation_keep_their_class`,
  `rejected_edits_message_names_paths_and_bounds_the_list`.
- `[x]` **`wheat … --help | head` panics with `failed printing to stdout:
  Broken pipe`.** Human output should treat EPIPE as a normal exit. Fixed
  2026-09-24: every stdout write (help, human output, JSON success and error
  envelopes) goes through one writer that flushes. When the reader is gone,
  Wheat exits 141 (what a SIGPIPE death reports) and prints nothing. Stderr
  diagnostics no longer use `eprintln!`, so a closed stderr drops them instead
  of panicking, and the exit status stays the same. Tests:
  `crates/wheat-cli/tests/broken_pipe.rs`.
- `[x]` **`wheat diff` cannot be limited to paths.** It takes only `--revision`,
  `--from` and `--to`, so `wheat diff PATH` is rejected. Accept optional path
  arguments that filter every diff mode, like `git diff -- PATH`. Fixed
  2026-09-24: `wheat diff [PATH]...` takes repository-relative files or
  directories (one trailing `/` allowed) and filters the workspace, `--revision`
  and `--from/--to` modes. The work is cut as well as the output. The Merkle diff
  cursor (`TreeDiffCursor::restricted_to` with `TreePathFilter`) never opens a
  directory that cannot hold a selected path, and the change limit counts only
  reported changes. The workspace scan does not read unselected files. Invalid
  paths exit 2 with `invalid diff path`. Tests:
  `crates/wheat-cli/tests/diff_paths.rs`, plus
  `path_filter_matches_roots_and_below_and_reaches_their_ancestors` and
  `restricted_diff_reports_selected_paths_and_skips_other_directories` in
  `crates/wheat-store/tests/merkle_tree.rs`.
- `[x]` **wheat-core's reference model and wheat-store author revisions against
  different views.** `Repository::add_revision` (`wheat-core/src/repository.rs`)
  authored against `dependency_view`, the composition of the change's ancestors
  only. `PersistedRepositoryMutator::revise` authored against the change's current
  application target, the view after every change before it in canonical order,
  which includes unrelated earlier siblings. Fixed 2026-09-25: every revision is
  authored against its spot (the view after every change before it in canonical
  order), in core, store create, revise, and squash (`5dffcfd`). `wheat add
  --change B` now captures the edit against the view the workspace shows and
  places it at B's spot. Blocks of later changes that the edit overlaps (or a
  whole file a later change creates) move into B, after a `[y/N]` prompt, or with
  `--force` when there is no terminal or the output is JSON (error code
  `edits_would_move`). This also fixes B silently taking over every later
  change's edits to a file it captured. Tests: `crates/wheat-cli/tests/edit_move.rs`,
  `crates/wheat-core/src/edit_placement/tests.rs`,
  `crates/wheat-store/tests/later_edits.rs`.
