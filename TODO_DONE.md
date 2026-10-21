# TODO — done

Finished items, moved here from `TODO.md` under the same headings.

## Found while dogfooding (2026-10-19)

- `[x]` **`structural_delta::one_step_forward_yields_little_more_than_the_difference`
  fails about 1 run in 20** (failed CI on 37d047f). Change IDs are UUIDv7, so the
  radix maps' shape depends on which millisecond each change was made in. When two keys
  straddle a byte boundary, a new view's map splits a node that its old counterpart
  keeps whole, and the walk sent the split-off node again. Done 2026-10-20 by fixing
  the walk, not the test. There were two causes:
  - A revised or resolved view paired with its nearest old ancestor. Now it pairs
    with the old view it replaces, which is the old child of its parent's counterpart
    ending in the same change, and whose maps split the same way.
  - A radix node shared by two collections, pruned as aligned in one, was walked and
    sent again in the other. Now anything pruned as aligned counts as old everywhere.

  `one_step_forward_yields_the_difference_however_change_ids_split_the_maps` covers
  48 fixed ID layouts. On the old walk, at least 30 of 96 such layouts failed. The 3,000-run
  stress of the random test passes. `fetch_incremental` gained a revise step: 9.5
  instead of 11.5 objects per recomposed view, for 19.0 instead of 17.6 store reads.

- `[x]` **`wheat push` silently drops changes ordered before an imported tip
  (2026-10-21).** After an import, a `wheat change new --independent` change sorts
  before the imported ones (UUIDv7 below UUIDv5), so the view is ordered
  `independent, first, second`. Push matched the remote tip to `second` by its
  recorded Git ID and reused it, skipping every application before it, printed
  "already at ...", and recorded the reused commit for the new application, so the
  next push failed as `corrupt_repository`. The export now reuses an anchor only
  when the chain up to it is exactly the anchor's first-parent history (each entry
  known by provenance or this store's projection mapping); otherwise it is
  `UnmatchedAnchor`, and push exports afresh and refuses to rewrite the remote.
  This applies to tag pushes too. Test:
  `a_change_ordered_before_imported_history_is_never_dropped_from_a_push`.

- `[x]` **Git is one-way after the import.** Commits pushed to the Git remote by
  anyone else could not be brought into an existing Wheat repository, and `wheat
  push` then refused. Done 2026-10-21: `wheat pull <git-url>` (`--from <ref>`,
  default the label's imported Git ref) walks back along first parents from the
  remote tip to the newest commit the workspace holds, imported or pushed by this
  store (new commit-keyed refs in the projection), appends the commits above it as
  the importer would, and moves local work above them like `git pull --rebase`, with
  conflicts kept as durable conflicts. The next push fast-forwards. The push refusal
  now says to pull and is `git_remote_diverged`, `retryable: false`. Tests:
  `git_round_trip.rs`, `git_pull.rs`; bench case `git_pull_incremental`; design in
  `docs/design/wheat-local-cli.md`, "Pulling from Git".

- `[x]` **Losing the Git projection duplicates pushed changes on the next pull
  (2026-10-21).** The commit refs in `projections/git.git` were the only record of
  which Git commit a native change was pushed as. Done 2026-10-21: Bart chose to
  record "application A projects as commit C" in the repository state, as a
  `ProjectedCommitSet` (20-byte SHA-1 commit then application ID). Push records the
  commits before pushing them. The set travels over `wheat://`, is unioned, validated
  against the retained applications, and filtered by squash. The projection's
  `refs/wheat/applications/*` and `refs/wheat/commits/*` are gone, and so are the tree
  re-hash checks. SHA-256 Git repositories are not supported. Tests:
  `wheat-store/tests/projected_commits.rs`,
  `pushed_commits_are_recognised_after_the_projection_is_lost`,
  `another_store_recognises_commits_pushed_from_the_store_it_cloned`.

- `[x]` **Git push and pull re-project a whole tree each time (2026-10-21).** Push
  checked its reused anchor, and pull a commit it recognised as pushed, by writing the
  Wheat tree into the projection again, which read every file. Done 2026-10-21. The
  projected commits replace both checks. The first tree an export writes is patched
  from the nearest projected commit at or below the anchor, and each later one from
  the commit before it: unchanged subtrees and files keep their Git entries, so it
  reads only the paths changed since. The chain walk follows each view's last change
  instead of scanning every application of every view, which was O(n²). On
  `git_pull_incremental`'s 2,000-commit side, a push after a pull went from 17.0 s to
  2.1 s, mostly full validation, and read 21 contents and 6 tree nodes instead of
  2,001 contents. Push's JSON reports `export.trees_read` and
  `export.contents_read`; the bench budgets them. Test:
  `a_projected_anchor_seeds_the_next_tree_so_only_changed_paths_are_read`.

## Found while scoping the Scion lessons (2026-10-15)

- `[x]` **`catalog.head` and `repack.intent` are hand-encoded.** Both are BinSchema
  records now, along with six more hand-built layouts the new guard test found: the
  repository head, the content envelope header, the snapshot and ownership manifests,
  the materialization progress record and the Git import source. The content
  generation fingerprint and index digest hash BinSchema encodings too. BinSchema's
  Rust decoder now caps an array reservation by how many items the bytes left can
  hold, so the hand-read count checks before decoding are gone.
  `crates/wheat-core/tests/no_hand_rolled_encodings.rs` fails on any new
  `to_/from_*_bytes` or `byteorder` use outside generated code and tests.
- `[x]` **The server ignores `publish_key`.** A publish whose CAS succeeded but whose
  reply was lost came back `stale` on retry. Now (2026-10-20) the repository head
  records each publish's receipt, keyed by `publish_key` and principal, in the write
  that moves it (24 hours, at most 1024). A retry gets `already_committed` with the
  first sending's head, or waits for it while it is written; a key reused for a
  different publish is refused. `wheat publish` resends once over a new connection
  when the connection is lost. Tests: `group_commit_test` (retries during and after
  the write, a reused key), and `remote_publish`
  `a_publish_whose_answer_is_lost_is_sent_again_and_published_once` (a proxy drops the
  first `PublishResult`).

## Security and performance scan (2026-09-26)

- `[x]` **Uploads go one file per message (scanner only).** The client sent each
  blob as its own `ContentUpload`. Uploads now use `ContentBatch` and `ContentChunk`,
  built by the shared `ContentBatcher` in `wheat-protocol` (2026-10-20): a publish of
  2,000 small files went from 2,016 messages to 17. Contract in bench
  `remote_latency`. Streaming large blobs to disk stays open in `TODO.md`.

- `[x]` **Transition validation reads every added blob in full (2026-10-05).**
  `repository_transition.rs` (step 5) now asks `ContentStore::logical_length` whether
  each added content ID exists, without reading or hashing it (2026-10-20, 1b3c962):
  content is checked against its ID when it is stored.

- `[x]` **Content manifests are full snapshots, so comparing two still reads both
  (2026-10-05).** Publish and fetch now look up only the content the old head lacks,
  but finding it decodes both manifests' snapshots (about 3.3 MB each at 100,000
  files) and merges them. Canonical manifests may not be deltas
  (`ContentSetStore::update_batch`), so a cheaper comparison needs a different shape,
  such as a radix set. Done 2026-10-15: the aggregate is a radix content set again,
  so publish and fetch diff two of them and skip every shared subtree, and each
  view's content is a radix content map a commit adjusts by its tree diff. See
  `docs/design/wheat-incremental-validation.md`, step 2.

- `[x]` **About four fsyncs per file during materialization (confirmed).**
  `execute_materialization` synced the staged file, the install-intent progress
  record, the parent directory and the transaction for every file, one file at a
  time: `wheat import` of 4,101 small files took about 30 s at 8% CPU with about
  16,000 fsyncs. Done 2026-10-05: installs go in batches of up to 256 whose paths do
  not depend on each other. A batch's staged files are written and synced on up to
  32 threads, the batch gets one progress record ("intents up to N"), and each
  workspace directory it touched is synced once after it is applied. Deletions keep
  their own syncs. Staged files are first written under a `writing-` name and
  renamed once synced, which also fixes a crash while staging leaving a half-written
  file under its staged name that recovery then refused forever. Measured in
  release on NVMe ext4: importing 4,096 files went from 30.1 s to 1.3 s; at 20,000
  files `import` went from 145 s to 9.3 s and a switch replacing every file from
  175 s to 6.4 s. Most of what is left is the staged files' own syncs. Tests:
  `progress_storage_is_one_record_per_batch`,
  `recovery_discards_half_written_staged_leaves`,
  `a_batch_record_must_move_forward_by_at_most_one_batch`, and the existing
  recovery and restore tests. Bench `materialize_wide` budgets `import` and both
  switches.
- `[x]` **About four fsyncs per object when objects go in one by one (scanner only).**
  `apply_change_authoring` (`repository_mutation.rs`) and
  `RepositoryPublisher::prepare` (`repository_publication.rs`) call `put_if_absent` in
  a loop, so each object would be its own journaled active-segment commit. Checked
  2026-10-05: not real for the mutator. Every CLI caller (`add`, `change new`,
  `squash`, conflict resolution) hands it a `BatchObjectStore`, and `wheat add`
  makes 55 syncs for 1 file and for 20 (three packed-store commits, plus the head,
  the workspace pin and workspace metadata). `prepare` wrote straight to the store
  it was given (`wheat init`, bench fixtures); it now stores the closure with one
  `put_batch` (test `preparation_stores_the_closure_in_one_batch`). Two constant
  per-command costs found on the way are in `TODO.md`.
- `[x]` **A wall-clock assertion in a test (2026-10-05).**
  `representative_scale_update_is_linear_enough_for_interactive_use`
  (`crates/wheat-store/tests/content_set_policy.rs`) asserted the update takes under
  10 s. Done 2026-10-05: the merge is one pass over sorted members and edits, so
  what could grow is store traffic. The test, now
  `representative_scale_update_reads_and_writes_one_object`, counts it: one read and
  one write for a 10,000-edit update of 100,000 members. No other test asserts on
  wall time.
- `[x]` **`wheat status` rewrites the workspace pin (2026-10-05).** Registry
  reconciliation wrote and synced `workspaces/<id>.json` and its directory on every
  command, including read-only ones (about 4.6 ms on NVMe). Done 2026-10-05:
  `WorkspaceRegistry::publish` leaves an identical pin alone and only syncs the
  directory, which keeps an earlier rename durable if its sync was cut off (test
  `publishing_an_unchanged_pin_leaves_the_document_alone`).
- `[x]` **`MerkleTreeStore::apply_edits` re-reads the node it just wrote (scanner
  only).** `merkle_tree.rs` decoded the child node only to check whether it is
  empty. Confirmed and done 2026-10-05: also, a new directory was written as an
  empty node and read back before recursing. The recursion now works on decoded
  nodes: each existing directory on an edited path is read once, nothing written is
  read back, and directories left empty are never written. Test
  `apply_edits_reads_each_existing_directory_once`.
- `[x]` **One-by-one edit reads in import and authoring (scanner only).**
  `staged_import.rs` (`verify_mapping`) and `workspace.rs` read edits with
  `reader.edit(id)` per edit. Checked 2026-10-05: each edit is its own object, so
  this is one read per edit, linear, and the local packed store has no faster way
  to read many. Re-adding 20,000 files to one change spends about 0.1 s on it (plus
  85 ms in `load_edits`) out of 5.4 s; the rest is a new `TODO.md` item. Found on
  the way: adding to a file twice in one change failed with `VerificationFailed`
  when the second add was next to the first's insertion (fixed in `fix(core)`,
  e13ee2f).
- `[x]` **Non-Unix workspace reads follow symlinks (scanner only).** The
  `#[cfg(not(unix))]` path in `workspace_files.rs` checked `symlink_metadata` and then
  `File::open`ed, which races and follows symlinks. Every wheat command already stops
  with `unsupported_platform` off Linux, so the code could not run. Done 2026-10-05:
  that `platform` module now fails every read with `io::ErrorKind::Unsupported`, so
  it cannot be switched on by accident (test `every_read_is_refused_off_unix`, which
  runs only off Unix). Not compiled for a non-Unix target here: the only installed
  one, WASI, fails first in `signal-hook-registry`.
- `[x]` **Other non-Unix fallbacks check and then open, or do not lock (2026-10-05).**
  `workspace_registry.rs` and `rewrite_transaction.rs` refused a symlink with
  `symlink_metadata` and then opened by path, and their non-Unix `lock_exclusive` /
  `unlock` / `try_lock` (and `authoring_metadata.rs`'s) did nothing. Done 2026-10-05:
  wheat-cli's operating-system calls now live in one module, `sys.rs`, whose Linux
  half makes the no-follow, descriptor-relative calls and whose other half fails
  every one with `io::ErrorKind::Unsupported`. The registry, rewrite and
  materialization code call it with no `cfg` of their own, and every lock uses std's
  `File::lock` / `try_lock`, which work everywhere. The store and git crates got
  the same treatment, and the whole workspace now builds for `x86_64-pc-windows-gnu`
  (`just check-windows`, warnings fail). Tests: `sys::tests` (Linux behaviour, plus
  `every_call_is_refused_off_linux`, which runs only off Linux).
- `[x]` **Publish walks the whole content manifest (confirmed).** `handle_publish`
  (`publish.rs`, the content phase) looked up every content ID of the proposed
  envelope, even for a one-line change, and the client's `fetch_content`
  (`remote.rs`) did the same on every fetch. Done 2026-10-05: both look up only
  `ContentSetStore::added(old, new)`, the content the old head (the server's current
  head; the client's last fetched envelope) lacks. That relies on a head being kept
  only once all its content is stored, as transition validation already did. Tests:
  `content_is_asked_for_only_where_the_head_lacks_it` (`publish_test.rs`, fails on
  the old code), `content_checked` in `remote_fetch.rs`,
  `added_lists_only_new_members_across_snapshots_and_deltas`. Bench
  `fetch_incremental` records `content_checked` and budgets its growth. Finding the
  difference still decodes both snapshots (see `TODO.md`).
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
  `crates/wheat-protocol/tests/forged_lengths.rs`.
- `[x]` **`DeltaZstd` decode reserved 1 GiB per patch (confirmed).**
  `zstd::bulk::decompress(patch, HARD_LOGICAL_RECORD_BYTES)` reserves the whole
  capacity up front, so a 16-deep delta chain could ask for 16 GiB. Done 2026-09-26:
  both decode paths go through `decompress_delta_patch`, which needs one whole frame
  that declares its size, refuses a size above the target length, and reserves only
  that. The writer already kept patches shorter than their target; it now refuses a
  longer one too. Test: `delta_patches_are_bounded_by_their_target_before_decompressing`
  in `crates/wheat-store/src/codec.rs`.
- `[x]` **Content transfers had no limits (confirmed).** The client's `ContentChunk`
  loop ignored `offset`, had no size cap and took content it never asked for; publish
  held every uploaded blob until `ContentUploadComplete`, took any ID, dropped
  multi-chunk uploads with a warning, and the client could not publish a blob over one
  16 MiB message. Done 2026-09-26: both ends receive through
  `wheat_protocol::ContentReceiver` (requested IDs only, each once, contiguous chunks,
  at most `MAX_CONTENT_SIZE` = 1 GiB, nothing missing at the end); the server stores
  uploads in batches of up to 64 MiB as they arrive and refuses a broken upload with a
  rejected `PublishResult`; the client sends blobs over 4 MiB as
  `ContentUploadChunk`s. Tests: `content_receiver` unit tests,
  `publish_reassembles_chunked_content` and six `publish_refuses_*` tests in
  `crates/wheat-server/tests/clone_fetch_publish_test.rs`,
  `crates/wheat-cli/tests/remote_large_content.rs` (a 17 MiB file through `publish`
  and `clone`). Benchmark: `publish_content` (server peak 210 MiB for a 384 MiB
  publish, flat at twice the size; budget 256 MiB).
- `[x]` **Directory symlinks get the wrong type (checked: not a bug).** The
  `is_symlink && is_dir` branch in `workspace_walk.rs` could never run, but what runs
  instead is right: like Git, a symlink to a directory is one file entry, is not
  descended, and `dir/` ignore rules do not match it (checked against
  `git status --ignored`). Done 2026-09-26: the dead branch is gone, with a comment
  saying why, and `symlink_to_directory_is_a_file_entry` in
  `crates/wheat-cli/tests/untracked_status.rs` pins the behaviour.
- `[x]` **File descriptors leak on error (confirmed).** `cleanup_orphan_transactions`
  and `ensure_progress_only` in `materialization.rs` `libc::dup`ed a descriptor and
  closed it by hand after a `?` that could return first. Done 2026-09-26: all three
  listings go through `read_open_directory`, whose duplicate is an `OwnedFd` from
  `try_clone_to_owned` (also close-on-exec, which `dup` was not). Test:
  `transaction_listing_closes_its_descriptor_on_error`.
- `[x]` **Unchecked addition in delta apply (checked: not reachable).**
  `written + size` could not overflow: `written` never passes the target length and
  one instruction adds at most 16 MiB. Done 2026-09-26 anyway, to match the checks
  around it: both use `checked_add`. Test: `instructions_cannot_write_past_the_result`
  (nothing covered those two errors before).
- `[x]` **Accidental O(E²) in staged-import verification (confirmed).**
  `verify_mapping` (`crates/wheat-git/src/staged_import.rs`) found each replay
  outcome's edit with a linear search, and also looked every touched path up from the
  root one at a time, decoding a wide directory again for each file in it. Profiling
  showed the lookups were the bigger half. Done 2026-09-26: a binary search over the
  sorted edits, and one shared `path_shapes` walk per tree. Benchmark:
  `import_resume_wide` (resume after a 100,000-file commit: 20.8 s → 2.2 s, budget
  6 s). Tests: `resuming_one_commit_at_a_time_accepts_every_valid_mapping` and
  `resuming_after_a_tampered_mapping_is_refused` in
  `crates/wheat-git/tests/import_modes.rs` (nothing checked the resume either way
  before). The first test found a real bug: a Git commit that turns a directory into
  a file could not be imported at all. The result checks in `verify_mapping` and in
  `RepositoryVerifier::replay` (`repository_verify.rs`) looked up the deleted paths
  below the new file and failed with `FileInPath`; such a path is now read as absent.
- `[x]` **Fetch always sends the whole repository (confirmed).** `handle_fetch`
  (`crates/wheat-server/src/fetch.rs`) collected the structural closure of all history
  in memory (`collect_structural_closure`), ignored `since_envelope`, and could build
  batches over the 16 MiB message cap; the client sent `FetchComplete` while wants
  were still in flight, so clones over about 512 objects came out incomplete. Done
  2026-09-26: both ends run one walk, `StructuralDelta`
  (`crates/wheat-store/src/structural_delta.rs`), that yields the objects of the new
  envelope the old one does not cover. The server pushes the walk from the client's
  last fetched envelope (`FetchPushEnd`), the client runs it over its own store and
  pulls any gaps (`ObjectWant`, exactly one `ObjectWantReply` each), and objects over
  8 MiB go as `ObjectChunk`s. `closure.rs` is gone. The client stores fetched blobs in
  batches with 8 writers instead of one at a time (a 4,101-file clone: 141 s → 30 s
  in debug). Tests: `crates/wheat-store` walk tests,
  `crates/wheat-server/tests/fetch_test.rs`, `crates/wheat-cli/tests/remote_fetch.rs`.
  Benchmark: `fetch_incremental` (one change after 10 and after 200 changes: 20 → 26
  objects, 33 → 43 store reads; budget 150%). Sending deltas instead of whole objects
  is still open (see "Loose ends").
- `[x]` **Publish asked for at most 4,096 missing blobs, once (confirmed).** Done
  2026-09-26: the server asks in rounds of at most 4,096 (`ContentNeeded`, answered by
  one `ContentUploadComplete` each) and says `ContentComplete` when it has them all.
  Test: `more_new_files_than_one_content_request_publish_and_clone` in
  `crates/wheat-cli/tests/remote_large_content.rs`. Walking the whole manifest is still
  open.
- `[x]` **Publish could move the head with objects missing (found 2026-09-26).** The
  server rediscovered missing objects after every `ObjectBatch` (racing the client's
  other batches), dropped its frontier past 4,096 IDs, followed references with the
  incomplete `extract_object_references`, and never checked completeness before
  moving the head; a 4,101-file publish left the server's store invalid. Done
  2026-09-26: the server runs `StructuralDelta` from its head to the proposed
  envelope, sends each missing batch as one `ObjectsNeeded` round, accepts exactly
  those objects (whole or as `ObjectChunk`s, each once, nothing else), and goes on only
  when all have arrived. The client sends objects over 8 MiB in chunks. Tests:
  `crates/wheat-server/tests/publish_test.rs` (a publish of more than one round leaves
  the server complete; chunked objects; six refusals), and
  `an_object_splits_into_chunks_the_receiver_puts_back_together` in
  `crates/wheat-cli/src/remote.rs`.
- `[x]` **Fetch treated local and remote generations as one (found 2026-09-26).** A
  fetch set the local head's generation to the remote's, but the local head only moves
  one generation at a time, so any fetch over more than one remote publish failed; and
  after publishing two local changes at once, fetch said there was unpublished work.
  Done 2026-09-26: the local head counts its own generations, a fetch moves it one
  generation to the remote envelope, and unpublished work is a local head envelope
  other than the one last fetched or published. Test:
  `a_fetch_catches_up_on_several_publishes_and_knows_its_own` in
  `crates/wheat-cli/tests/remote_fetch.rs`.

## Loose ends found in the design-doc audit (2026-09-25)

- `[x]` **`one_step_forward_yields_little_more_than_the_difference` fails about one
  run in twelve (2026-09-26).** The same flake, found again on 2026-10-19 and fixed
  on 2026-10-20 by better pairing; see "Found while dogfooding (2026-10-19)".
- `[x]` **Uploaded content is hashed four or five times on the server (2026-09-26).**
  Measured at six times on a loose server. Done 2026-10-20: content is hashed where
  it comes in, which makes it a `VerifiedContent` (wheat-core). The stage,
  `ContentItem`, every `ContentStore` put and the content backends take it and don't
  hash it again; the packed catalog takes a `CheckedRecord` and no longer validates
  each record twice. Transition validation checks that new content exists from stored
  metadata instead of reading and hashing it, and `ContentStore::contains` on a
  backend no longer reads the content at all. No blob above the loose threshold is
  cloned on its way into a packed store. Contracts: `a_published_blob_is_hashed_once`
  (wheat-server) and `verified_content_is_stored_without_hashing_it_again`
  (wheat-store), counting `content_bytes_hashed()`.

- `[x]` **`wheat integrate` refuses a frontier that revises its label's view
  (2026-10-09).** Done 2026-10-15. Integrate now starts the joined view below the
  lowest revised change on the label's composition path and composes the label's
  changes above it again (`reapplied` in the outcome and the CLI JSON). Finding that
  start reads the revised changes' applications, not the history below them. Fixing it
  turned up three more bugs, fixed in the same commit:
  - integrate composed the frontiers' changes on top of the label's view even when
    canonical order puts one below some of the label's changes (a change created with
    `--independent` or `--depends-on` before them). Revising a label change afterwards
    rebuilt from a view that lacked the joined change and dropped it. Integrate now
    also starts below the first label change canonical order puts after a joined
    change, found with a new seek, `RevisionMapStore::cursor_from`, over the label's
    changes with higher ids;
  - revising a change the basis holds (`wheat resolve` on a conflict in a label's
    view) left the result outside the label's descendants: the continuation edge was
    skipped when the effective view was the basis. The resolution vanished from
    `frontier list`, and the next pull switched past it;
  - revising the first change in canonical order composed from the basis view, which
    can already hold it, so a label change was applied twice. `prefix_view(0)` is now
    the view the first change was applied onto.
  Tests in `crates/wheat-store/tests/repository_integration.rs` (including the read
  count of `integrating_a_revised_basis_change_reads_from_the_change_up`),
  `revision_map.rs` (`a_cursor_from_a_bound_yields_the_keys_from_it_on`) and
  `pull_takes_a_resolution_of_a_conflict_the_label_holds` in the CLI.
- `[x]` **The `.gitignore` fallback diagnostic points at a missing command.** Done
  2026-10-09: `wheat ignore import` (`crates/wheat-cli/src/ignore_import.rs`), see
  `wheat-local-tooling.md`.
- `[x]` **Other `lookup_many` callers may fail when a directory became a file.** Done
  2026-10-09. Checking it with the CLI found that a directory and a file could not
  trade places in one revision at all:
  - the workspace reader failed on `dir/x` once `dir` was a file, and on `node` once
    it was a directory, so `status`, `diff` and `add` all errored. Capture and
    status now read both as the file deleted (`absent_where_shape_changed`);
    materialization still refuses them as before;
  - edit application used `apply_edits`, which refuses an edit set touching both
    `dir` and `dir/x`; it now uses `apply_final_edits` (as import does);
  - capture settlement left every shape change to `materialize` and reported the
    paths as diverged; it now records them when the workspace already has the new
    shape, falling back to the old behaviour if the pin still holds files the
    capture did not name.
  `lookup` and `lookup_many` read a path below a file as `None`. Test
  `a_directory_and_a_file_trade_places_in_one_revision` in `workspace_add.rs`. Bench
  `add_scale` unchanged (results `20261009-022703` and `20261009-022720`).
- `[x]` **A content fetch can hang the client.** Done 2026-10-09:
  `handle_content_fetch` reads the requested blobs together and, if that fails,
  one at a time; a blob over 4 MiB goes as `ContentChunk`s on either path, and one
  it cannot read ends the operation with an `Error` (404 when missing, 500
  otherwise) naming the blob. Tests in `crates/wheat-server/tests/fetch_test.rs`.
- `[x]` **WebSocket messages up to 64 MiB are read before the 16 MiB check.** Done
  2026-10-09: server and client set `max_message_size` and `max_frame_size` to
  `MAX_MESSAGE_SIZE`. The frame limit already was 16 MiB, so the gap was a message
  of several frames. Tests send a 16 MiB fragment plus one more byte and check the
  receiver gives up without waiting for the end (`protocol_test.rs`, and
  `remote::connect_tests` in wheat-cli); both waited before the fix.
- `[x]` **A failed seal after publish leaks its message.** Done 2026-10-09, along
  with a worse problem in the same place: the server moved the head first and
  sealed after, so on a store that buffers objects (the S3 packed store) a failed
  seal or a crash left the head naming objects that were never stored. The seal now
  runs before the head moves, and a failure is a publish error with the head left
  alone. Test `a_publish_whose_objects_cannot_be_sealed_leaves_the_head_alone`.
- `[x]` **Stale `#[allow(dead_code)]` markers in `workspace_walk.rs`.** Done
  2026-09-26: removed. The one field they hid that nothing read,
  `gitignore_fallback_paths`, is gone too; the ignore stack still has the paths.
- `[x]` **`extract_object_references` is incomplete and no longer used.** Done
  2026-09-26: deleted from `crates/wheat-core/src/wire.rs`, with its two tests and
  its re-export. It missed several references, and nothing but its own tests called
  it after publish moved to `StructuralDelta`.
- `[x]` **Snapshot export may skip retained-set members.** Done 2026-09-26: it did.
  A snapshot of a 150-change history left out views, revisions, trees and more,
  because set members stored inline in a node were only followed when the node had a
  key of its own, which such a node never has. Fixed by taking every reference from
  `crates/wheat-store/src/object_graph.rs`, which `StructuralDelta` uses too. Tested
  by `a_snapshot_export_lists_the_same_closure_as_the_walk` and the long-history test
  in `crates/wheat-store/tests/structural_delta.rs`. The scratch index no longer
  syncs to disk on every write, which took that test from 67 s to 6 s.
- `[x]` **Snapshot export scans its whole scratch table for every object.** Done
  2026-09-26: `ClosureWalk` (`crates/wheat-store/src/snapshot.rs`) now keeps what is
  left to do in two stacks (`pending`, `pending_contents`) and pops the top row with
  one indexed `DELETE ... RETURNING`, with every statement cached. Bench case
  `snapshot_export` (10,000 vs 40,000 files, 50 changes each, median of 3): time per
  item for the large repository is 107% (export) and 108% (restore) of the small
  one's, budget 150% (`bench/results/wheat-quick/20260926-134434-7fe1d5c5de7c.tsv`).
  One run under heavy load measured restore at 191%. No
  before-numbers: they need the old code built next to the new. Growing files and
  changes together is not a fair size test, because every view keeps a list of all
  its files: 4× both gave 15× the bytes.
- `[x]` **Snapshot export refuses a radix node that sits under two edges.** Done
  2026-09-26: the walk visits an object once per place it is reached (a `visits`
  table keyed by object and place: a plain reference, or a radix node's key path),
  and records it on the first visit. A crafted map can repeat one subtree at every
  level, so visits are capped at twice `max_objects` (`LimitExceeded("closure
  visits")`). Tests in `snapshot.rs`: `one_subtree_under_two_edges_is_followed_under_both`
  and `repeated_subtrees_are_capped_by_the_visit_limit`. The fetch walk still lacks
  such a cap (new item in `TODO.md`).

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
- `[x]` **The four retained-set updates in `finish_mutation` cost about 0.85 s.**
  `radix::Store::update_batch` applied a batch one edit at a time, with a lookup and a
  full root-path rewrite per edit, so a batch of ~500 keys wrote ~500 roots. Its
  batched path was bypassed because it couldn't update the old node when a prefix
  split. Done 2026-10-15 (7551122): one pass per batch, each changed node written
  once. Contract: `a_batch_writes_each_changed_node_once`
  (`crates/wheat-store/tests/collection_builder_parity.rs`). `deep_stack` release
  run: `bench/results/wheat-quick/20261015-063823-755112269a29.tsv`. The previous
  run is 0581f94, so the comparison spans other commits too.

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
