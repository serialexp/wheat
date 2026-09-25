# Wheat Phase 2 — Local Tooling Design

Status: partial — Phase 0 discovery and Phase 1 ignore-aware status landed; the
extended configuration, `--exclude`, `add --all` follow-ups, hooks, and remotes
are outstanding
Owner: bart
Last updated: 2026-09-25

## Implementation status

Tracking the gap between this design and what's on `main`. Tick items as they
land; move whole sections to "done" once everything inside them is checked.

### Done

- [x] **Phase 0 — repository discovery.** Walk the directory hierarchy upward from
  the working directory to find the `.wheat` control directory, stop at filesystem
  boundaries, and expose the resolved workspace/store paths to every subsequent
  command without requiring `--repository`. (`crates/wheat-cli/src/discovery.rs`
  `discover_workspace`: stops at a `st_dev` change, refuses a symlinked `.wheat`,
  and checks the workspace's store identity against the store. Test:
  `untracked_status::discovery_from_subdirectory`.)
- [x] **Phase 1 — ignore evaluation engine.** Implement `.gitignore`-compatible
  pattern matching: anchored and unanchored globs, directory-only patterns,
  negation, and comment lines. The engine is a shared library used by status,
  capture, diff, and any future workspace-scanning command. Evaluation is bounded
  and must not follow symlinked directories. (`crates/wheat-ignore`. Its only
  consumer today is `wheat status`. The walk is bounded by
  `workspace_walk::WalkLimits`; the ignore-file and pattern-count bound is still
  outstanding below.)
- [x] **Phase 1 — `.wheatignore` file loading (repository and user-global
  sources).** Nested, directory-scoped `.wheatignore` files and the user-global
  `${XDG_CONFIG_HOME}/wheat/ignore`. Tracked paths remain visible even when a rule
  matches them. (Tests: `untracked_status::nested_wheatignore_overrides_parent`,
  `global_ignore_file`, `tracked_paths_remain_visible_despite_ignore_rule`.) The
  `--exclude` source is outstanding below.
- [x] **Phase 1 — `.gitignore` fallback with diagnostic.** When a directory
  contains `.gitignore` but no `.wheatignore`, wheat reads the `.gitignore` at the
  lowest precedence beneath repository-versioned rules and prints a one-line
  diagnostic noting the fallback. The diagnostic should suggest creating a
  `.wheatignore` (or running `wheat ignore import`) to make the rules explicit and
  silence the message. A `.wheatignore` in the same directory fully replaces its
  sibling `.gitignore` — they are not merged. (Tests:
  `untracked_status::gitignore_fallback_with_diagnostic`,
  `wheatignore_replaces_gitignore_in_same_directory`. The suggested
  `wheat ignore import` command does not exist yet; see Outstanding.)
- [x] **Phase 1 — workspace-wide status with ignore rules.** Extend `wheat status`
  to traverse untracked workspace paths, classify them as untracked or ignored, and
  bound the scan. This is the first consumer of the ignore engine and gates
  workspace-wide capture. (Status schema version 2 with `untracked` and
  `diagnostics`; symlinked directories are not followed. Tests in
  `crates/wheat-cli/tests/untracked_status.rs`.)
- [x] **Phase 2 — workspace-wide capture (`wheat add --all`).** `--all` selects
  all modified and deleted tracked paths instead of explicit path arguments, and
  excludes new untracked files. (Tests in
  `crates/wheat-cli/tests/workspace_add_all.rs`.) The `--include-untracked` flag
  and the pre-mutation capture plan are outstanding below.

### Outstanding

- [ ] **Phase 0 — extended configuration model (partial).** `core.editor`,
  `core.pager`, `core.default_label`, and `core.default_format` can be stored with
  `wheat config set`, but no command reads them yet. Keys for compatibility
  remotes, ignore overrides, and hook declarations do not exist. Repository
  configuration takes precedence per key over user configuration, as the existing
  actor identity contract already does.
- [ ] **Phase 1 — command-local `--exclude`.** `wheat_ignore::stack::IgnoreStack`
  already accepts exclude patterns, but `wheat status` passes none and no command
  exposes an `--exclude` flag.
- [ ] **Phase 1 — ignore-file and pattern-count bounds.** The walk has entry,
  directory, and depth limits with a truncation diagnostic, but the number of
  loaded ignore files and patterns is not bounded, and no test covers truncation
  (acceptance criterion 9).
- [ ] **Phase 1 — `wheat ignore import`.** The `.gitignore` fallback diagnostic
  tells the user to run this command, but it does not exist yet.
- [ ] **Phase 2 — `add --all` follow-ups.** Add `--include-untracked` (the help
  text currently says to name untracked paths explicitly instead), and display or
  emit the capture plan before mutation in interactive flows.
- [ ] **Phase 2 — lifecycle hook execution.** Implement the hook runner: resolve
  registered executables from `.wheat/hooks/<event>`, enforce timeout and output
  bounds, pass structured JSON context on stdin, collect exit status and optional
  structured JSON output on stdout, and report results to the user or automation
  caller.
- [ ] **Phase 2 — hook management commands.** Implement `wheat hook add`,
  `hook list`, `hook enable`, `hook disable`, and `hook remove` as the primary
  interface for hook registration. Commands write to the `hooks` sub-object in
  repository configuration.
- [ ] **Phase 2 — hook event coverage.** Register and fire hooks for the initial
  lifecycle events: `pre-capture`, `pre-publish`, `post-publish`,
  `pre-materialize`, `post-materialize`, and `pre-push`. Each event defines its
  structured input contract.
- [ ] **Phase 3 — hook composition and policy.** Define ordering, concurrency, and
  failure semantics when multiple hooks register for the same event. Decide whether
  repository-declared hooks require explicit local trust before execution. Define
  bypass auditing — a `--no-hooks` flag must record that hooks were skipped rather
  than silently omitting them.
- [ ] **Phase 3 — compatibility remote configuration.** Add a persistent named
  remote to repository configuration (`wheat remote add <name> <git-url>`) so
  `wheat push` and future `wheat import --from <remote>` need not repeat the URL.
  The remote is identified as a Git projection endpoint; it is not native
  synchronization.
- [ ] **Defer — editor and pager integration.** Launch a configured or detected
  editor for `wheat change new` without `-m`, and page long output through a
  configured pager. Deferred until the core tooling slice is proven.
- [ ] **Defer — alias and extension commands.** Allow user-defined command aliases
  and external `wheat-<name>` extension discovery in `PATH`. Deferred until the
  core command set is stable enough that aliases will not mask future built-in
  names.

## Why this exists

The Wheat local CLI has a complete Phase 1 single-target edit loop and Phase 2
label/frontier switching, but workspace-wide operations require supporting
infrastructure that does not exist yet. You cannot ask "what's untracked?" without
ignore rules, you cannot capture a whole working tree without knowing what to skip,
and you cannot run formatters or linters without a hook mechanism. Repository
discovery, configuration, and compatibility remotes are similarly absent — every
command currently requires an explicit `--repository` or must be run from the
workspace root.

These four concerns — ignore, hooks, discovery, and configuration — interact
tightly enough that designing them together avoids contradictions and redundant
file formats. This document specifies the local tooling layer that sits between
the Wheat model/store and the developer's working environment.

The parent architecture remains
[`server-native-source-control-store.md`](./server-native-source-control-store.md).
The local CLI contract lives in
[`wheat-local-cli.md`](./wheat-local-cli.md). This document refines the
outstanding Phase 2 tooling items from both.

## Goals

- Make `wheat status` and `wheat add` workspace-wide without requiring explicit
  path arguments for every file.
- Give developers a familiar, bounded, composable hook mechanism that is strictly
  better than Git hooks without being incompatible with existing tooling.
- Let every wheat command discover its repository from the current directory
  without `--repository`.
- Provide a single, layered configuration model that covers identity, ignore
  patterns, hooks, compatibility remotes, editor, pager, and output preferences
  without inventing a new format per concern.

## Non-goals

- Replacing the `.gitignore` specification. Wheat claims `.gitignore` pattern
  compatibility; it does not extend the syntax or redefine matching semantics.
- Implementing native remote synchronization. Compatibility remotes configure Git
  projection endpoints. Native clone/fetch/publish belongs to the protocol phase.
- Defining forge-side hook policy, webhook delivery, or CI integration. Server-side
  lifecycle events are a separate concern from the local hook runner.
- Treating hooks as a correctness boundary. Repository publication revalidates
  native invariants independently; a skipped or failed hook cannot produce a
  corrupt repository.
- Submodule or nested-repository support. Discovery stops at the first `.wheat`
  directory encountered and does not recurse into child repositories.

## Repository discovery

Discovery walks the directory hierarchy upward from the process working directory
(or an explicit `--directory` override) looking for a `.wheat` control directory.
The walk stops at:

- A `.wheat` directory — the containing directory is the workspace root, and
  `.wheat` holds the store or a pointer to a separate store.
- A filesystem boundary (different `st_dev`) — prevents accidentally claiming
  authority over a mount point or network share.

There is no upward scan past the first `.wheat` hit. Wheat does not support nested
repositories or submodules. A workspace whose `.wheat` directory has been removed
or renamed produces a clear discovery-failure error with the searched path and
reason.

An explicit `--repository <path>` on any command bypasses discovery entirely and
uses that path as the store location, exactly as it does today. Discovery is a
convenience that resolves to the same internal contract.

### Separate-store discovery

When `wheat import` places the repository store outside the workspace (the
current separate-store model), discovery needs a stable pointer from the workspace
to the store. The existing `.wheat/store` symlink or metadata file serves this
purpose. Discovery resolves the pointer and validates that the store is readable
before any command proceeds.

## Configuration model

### Existing contract

The Phase 1 CLI already has:

- **Repository scope:** bounded versioned JSON at `<store>/config.json`.
- **User scope:** bounded versioned JSON at
  `${XDG_CONFIG_HOME:-~/.config}/wheat/config.json`.
- **Precedence:** repository values replace user values per key.
- **Mutation:** `wheat config set [--global] <key> <value>` writes one typed key
  atomically.

### Extended keys

The following keys extend the existing schema. Each key documents its type,
default, and which scope is meaningful.

```text
actor.name          string    (repo, user)   — already implemented
actor.email         string    (repo, user)   — already implemented
core.editor         string    (repo, user)   — editor command for interactive input
core.pager          string    (repo, user)   — pager command for long output
core.default_label  string    (repo)         — default active label for new workspaces
core.default_format string    (repo, user)   — "human" or "json"; default "human"
```

Hooks and remotes use structured sub-objects rather than flat keys:

```json
{
  "hooks": {
    "pre-capture": {
      "command": ".wheat/hooks/pre-capture",
      "timeout_seconds": 30,
      "enabled": true
    }
  },
  "remotes": {
    "origin": {
      "url": "git@github.com:user/repo.git",
      "kind": "git-projection"
    }
  }
}
```

Hook commands are resolved relative to the workspace root. Absolute paths and
`PATH` lookup are supported. Remote `kind` is always `git-projection` until a
native protocol exists.

### Hook management commands

Hooks are registered and managed through `wheat hook` rather than by hand-editing
configuration JSON:

```text
wheat hook add <event> <command> [--timeout <seconds>]
wheat hook list
wheat hook enable <event>
wheat hook disable <event>
wheat hook remove <event>
```

`wheat hook add pre-capture .wheat/hooks/pre-capture --timeout 60` writes the
corresponding entry to repository configuration. `wheat hook list` shows all
registered hooks, their event, command, timeout, and enabled state. `enable` and
`disable` toggle the `enabled` flag without removing the registration. `remove`
deletes the entry entirely.

The config JSON remains the storage — `wheat hook` is the interface. Users who
prefer editing JSON directly can do so; the commands and the file describe the
same state.

### Configuration files are not hooks

Configuration declares *what* runs. The hook *executables* live in `.wheat/hooks/`
or elsewhere on the filesystem. Configuration stores their registration,
timeout, and enablement. A hook executable without a configuration entry is
ignored; a configuration entry pointing at a missing executable is a startup
error.

## Ignore rules

### Pattern syntax

Wheat uses the complete `.gitignore` pattern syntax:

- Blank lines are ignored; `#` introduces a comment.
- A trailing `/` matches directories only.
- A leading `/` anchors the pattern to the directory containing the ignore file.
- A leading `!` negates a previously matching pattern.
- `*` matches anything except `/`; `**` matches zero or more directories.
- `?` matches any single character except `/`.
- Bracket expressions `[abc]`, `[a-z]`, `[!a]` have their standard meaning.

Wheat claims this syntax completely. Partial compatibility — supporting globs
but silently ignoring negation, for example — is worse than no compatibility
because it produces false confidence in ported rules. The implementation must
pass the same edge cases as Git's `wildmatch` for the patterns it accepts.

### Source precedence

Ignore rules are evaluated in the following order. A later source can negate a
match from an earlier source.

1. **Built-in rules.** Wheat always ignores its own control paths (`.wheat/`,
   and the packed-store authority paths `repository.head`, `storage/`). These
   cannot be negated.
2. **Repository-versioned `.wheatignore` files.** Scoped to the directory they
   appear in and its descendants. Nested files take precedence over ancestors,
   exactly as `.gitignore` nesting works.
3. **User-global rules.** `${XDG_CONFIG_HOME:-~/.config}/wheat/ignore`. One file,
   no nesting.
4. **Command-local exclusions.** `--exclude <pattern>` on any workspace-scanning
   command. These are ephemeral and not persisted.

Tracked paths — paths that exist in the currently materialized view — remain
visible in status and capture output regardless of ignore rules. A rule that
matches a tracked path suppresses it from "untracked" output but does not hide
its modifications. This matches Git's behavior and prevents tracked files from
silently disappearing when an ignore rule is added after import.

### `.gitignore` fallback

**Settled.** When a directory contains `.gitignore` but no `.wheatignore`, wheat
reads the `.gitignore` at the lowest precedence beneath repository-versioned rules.
Every time the fallback activates, wheat prints a one-line diagnostic:

```text
note: reading .gitignore (no .wheatignore found); run `wheat ignore import` to make rules explicit
```

The diagnostic is per-scan, not per-file — one message covers all fallback files
encountered during a single `wheat status` or `wheat add` invocation. In JSON
mode the diagnostic appears as a `"diagnostics"` array entry in the result
envelope rather than stderr prose.

A `.wheatignore` in the same directory fully replaces its sibling `.gitignore` —
they are not merged. Once every directory with a `.gitignore` also has a
`.wheatignore`, the fallback never activates and no diagnostic appears.

`wheat ignore import` is a convenience that copies each `.gitignore` to a sibling
`.wheatignore`, stripping Git-specific patterns (`.git/`, `.gitmodules`,
`.gitattributes`). It is not required — manual creation of `.wheatignore` files
works identically.

### Evaluation constraints

- Ignore evaluation must not follow symlinks into directories.
- The total number of loaded ignore files and total pattern count are bounded.
  Exceeding the bound is a diagnostic, not silent truncation.
- Patterns are compiled once per scan and reused across path classification.

## Lifecycle hooks

### Design principles

Wheat hooks combine three elements:

1. **Git-compatible executable shape.** Hooks are executables in `.wheat/hooks/`.
   A shell script, a Python script, a compiled binary — anything executable works.
   Existing pre-commit frameworks can be adapted by changing the hook directory.

2. **Structured context on stdin.** Wheat writes a versioned JSON document to the
   hook's stdin describing what is about to happen: which files changed, their
   content identities, file kinds, the active change, the actor, and any other
   event-specific metadata. A hook that ignores stdin works fine — this is strictly
   additive over Git hooks.

3. **Structured results on stdout (optional).** A hook that writes nothing to
   stdout communicates only through its exit code, exactly like a Git hook. A hook
   that writes a versioned JSON document to stdout can report per-file diagnostics,
   suggested fixes, and categorized outcomes that wheat displays richly or passes
   through to automation.

This gives a **compatible floor** (exit 0/1, ignore stdin/stdout) and a **native
ceiling** (structured context in, structured diagnostics out).

### Lifecycle events

The initial event set covers the operations that exist today. Events for label
advancement, frontier consolidation, and native synchronization will be added
when those operations are implemented.

| Event | Fires | Blocking | Structured input includes |
|-------|-------|----------|--------------------------|
| `pre-capture` | Before `wheat add` writes a revision | Yes — exit ≠ 0 aborts | Changed file paths, content IDs, kinds, change title |
| `pre-publish` | Before repository head CAS | Yes — exit ≠ 0 aborts | New revision ID, resulting view, complete edit summary |
| `post-publish` | After successful head CAS | No | Same as pre-publish, plus publication result |
| `pre-materialize` | Before view switch writes to workspace | Yes — exit ≠ 0 aborts | Source and target view IDs, planned mutations |
| `post-materialize` | After successful materialization | No | Materialized view, changed path summary |
| `pre-push` | Before Git projection push | Yes — exit ≠ 0 aborts | Git remote URL, ref, projected view |

**Blocking** means the operation waits for the hook and aborts on non-zero exit.
Non-blocking hooks run after the operation completes; their failure is reported
but does not revert the completed operation.

### Structured input contract

Every hook receives a single JSON document on stdin, version-tagged:

```json
{
  "version": 1,
  "event": "pre-capture",
  "workspace": "/path/to/workspace",
  "repository": "/path/to/store",
  "actor": { "name": "Bart", "email": "bart@serial-experiments.com" },
  "change": { "id": "...", "title": "Fix the widget" },
  "files": [
    {
      "path": "src/main.rs",
      "status": "modified",
      "kind": "regular",
      "content_id": "abc123...",
      "previous_content_id": "def456..."
    }
  ]
}
```

Fields vary by event. The `version` field allows future evolution without breaking
existing hooks. Unknown fields must be ignored by hooks (forward compatibility).

### Structured output contract

A hook that writes JSON to stdout produces a versioned result:

```json
{
  "version": 1,
  "outcome": "rejected",
  "diagnostics": [
    {
      "path": "src/main.rs",
      "line": 42,
      "severity": "error",
      "message": "unused import",
      "source": "clippy"
    }
  ]
}
```

`outcome` is `"passed"`, `"rejected"`, or `"warning"`. When a hook writes valid
JSON with `outcome: "rejected"`, wheat uses that as the refusal reason even if the
exit code is also non-zero. When stdout is empty or not valid JSON, wheat falls
back to exit-code semantics.

### Execution constraints

- **Timeout.** Configurable per hook, default 30 seconds. A hook that exceeds its
  timeout is killed and treated as a failure.
- **Output bounds.** Stdout and stderr are each bounded to 1 MiB. Excess output
  is truncated with a diagnostic.
- **Working directory.** The workspace root.
- **Environment.** `WHEAT_EVENT`, `WHEAT_REPOSITORY`, `WHEAT_WORKSPACE`, and
  `WHEAT_HOOK_VERSION` are set. The rest of the process environment is inherited.
  Wheat does not synthesize Git-compatible environment variables (`GIT_DIR`,
  `GIT_WORK_TREE`, etc.).
- **No implicit bypass.** There is no `--no-verify` flag. A `--no-hooks` flag
  exists but is recorded in the revision's publication metadata so that automation
  and review can observe that hooks were skipped. The skip record is an auditable
  fact, not a silent optimization.
- **Stderr.** Displayed to the user in human mode; captured and included in
  structured error output in JSON mode. Never interpreted as structured results.

### Hook trust

Hooks declared in repository configuration are executables that run with the
user's full local authority. Wheat does not sandbox them. Trust is tracked per
hook using a content hash of the executable file:

- **First encounter.** On first run of a repository-declared hook, wheat displays
  the hook content (inline if ≤10 lines, otherwise the path and content hash) and
  requires explicit consent before execution. Consent is recorded in user-scope
  configuration alongside the content hash.
- **Content change.** When a previously trusted hook's content hash no longer
  matches, wheat pauses before execution, shows the updated content (inline if
  ≤10 lines), and asks whether to re-trust. This catches intentional updates and
  unexpected modifications alike.
- **Locally added hooks.** A hook added by the local user via `wheat hook add`
  is trusted immediately — the user just chose to install it. Trust consent
  applies to hooks that arrive through repository configuration (e.g. after
  import or when a collaborator adds one).
- **Thin-wrapper warning.** When a hook file is a short shim (≤3 non-comment,
  non-shebang lines) that delegates to an external command, wheat prints a
  one-time diagnostic noting that the content hash only covers the shim itself,
  not the tool it calls. The user can dismiss this permanently for a given hook
  via a `"suppress_shim_warning": true` flag on the trust entry, stored alongside
  the content hash. The warning re-appears if the shim content changes (since
  re-trust already triggers at that point).
- **Consent storage.** Trusted hook entries live in user-scope configuration
  keyed by repository identity and hook event, storing the command path, its
  content hash, and optional suppression flags.

## Phasing

### Phase 0 — discovery and configuration

1. Implement upward directory walk to `.wheat` with filesystem-boundary detection.
2. Extend the configuration schema with the new keys listed above.
3. Remove the implicit requirement for `--repository` on commands that currently
   need it.

### Phase 1 — ignore rules and workspace-wide status

1. Implement the `.gitignore`-compatible pattern engine as a shared library.
2. Load `.wheatignore` files, user-global ignore, and command-local exclusions.
3. Implement `.gitignore` fallback (controlled by configuration).
4. Extend `wheat status` to report untracked and ignored paths with bounded
   traversal.
5. Add coverage: nested ignore files, negation, tracked-path visibility,
   directory-only patterns, symlink non-traversal, and pattern-count bounds.

### Phase 2 — workspace-wide capture and hooks

1. Extend `wheat add` with workspace-wide mode that respects ignore rules.
2. Implement the hook runner with structured stdin, stdout, timeout, and output
   bounds.
3. Register and fire `pre-capture`, `pre-publish`, `post-publish` hooks.
4. Add hook trust consent for repository-declared hooks.
5. Add coverage: hook timeout, structured input/output, non-zero exit refusal,
   empty-stdout fallback, consent lifecycle.

### Phase 3 — remaining hooks and compatibility remotes

1. Fire `pre-materialize`, `post-materialize`, `pre-push` hooks.
2. Implement hook composition ordering and failure semantics for multiple hooks
   on the same event.
3. Add compatibility remote configuration and integrate with `wheat push`.
4. Implement `--no-hooks` with auditable skip recording.
5. Add coverage: multi-hook ordering, skip audit trail, remote configuration
   validation.

### Deferred

- Editor and pager integration.
- Alias and extension command discovery.
- Server-side hook policy and webhook delivery.
- Native remote synchronization (protocol phase, not tooling).

## Acceptance criteria

### Phase 1 — ignore-aware workspace status

A black-box test must demonstrate:

1. `wheat status` in a workspace with `.wheatignore` suppresses matched untracked
   paths and reports non-matched untracked paths.
2. Nested `.wheatignore` files take precedence over ancestors.
3. Negation patterns (`!`) re-include previously excluded paths.
4. Tracked paths remain visible in status output even when matched by an ignore
   rule.
5. User-global ignore applies when no repository-scoped rule matches.
6. `--exclude` patterns override all other sources for the duration of one command.
7. `.gitignore` fallback produces the same classification as an equivalent
   `.wheatignore` when enabled, and is inactive when disabled.
8. Symlinked directories are not traversed during the untracked scan.
9. Pattern count and file count bounds are enforced; exceeding them produces a
   diagnostic, not silent truncation.
10. Discovery from a nested subdirectory resolves the correct workspace root and
    store.

### Phase 2 — capture and hooks

A black-box test must demonstrate:

1. `wheat add --all` captures all non-ignored modified and deleted tracked paths
   as a single revision; new untracked files are excluded unless
   `--include-untracked` is also passed.
2. A `pre-capture` hook receiving structured JSON on stdin can read the changed
   file list and reject the capture with structured diagnostics.
3. A `pre-capture` hook that ignores stdin and exits 0 permits the capture.
4. A `pre-capture` hook that exceeds its timeout is killed and the capture is
   aborted.
5. A `post-publish` hook receives the publication result and its failure does not
   revert the publication.
6. `--no-hooks` skips hook execution and records the skip in publication metadata.
7. A repository-declared hook that has not been consented to is not executed;
   wheat prompts for consent.

## Open questions for review

1. ~~`.gitignore` fallback~~ — **settled**: yes, with a per-scan diagnostic
   suggesting `wheat ignore import`.
2. ~~Hook declaration location~~ — **settled**: hooks live in the main config
   (`hooks` sub-object), managed via `wheat hook add/list/enable/disable/remove`
   so nobody needs to hand-edit the JSON.
3. ~~Hook stdin content~~ — **settled**: paths and content IDs only, not full
   file content. Hooks can read workspace files by path if they need the bytes;
   streaming content through stdin would constrain hooks that want to read
   selectively or in parallel.
4. ~~Workspace-wide add and untracked files~~ — **settled**: `wheat add --all`
   captures modified and deleted tracked paths only. New untracked files require
   explicit `--include-untracked` or explicit path arguments. Principle of least
   surprise: missing a file you wanted is "huh"; including a pile of files you
   didn't want is worse.
5. ~~Hook trust mechanism~~ — **settled**: trust is keyed on a content hash of the
   hook executable. When a previously trusted hook's content changes, wheat pauses,
   shows the hook content (inline if ≤10 lines, otherwise the path and hash), and
   asks whether to re-trust. This catches both intentional updates and unexpected
   modifications without forcing blanket re-consent on every edit.

## References

- [`docs/design/server-native-source-control-store.md`](./server-native-source-control-store.md)
  — canonical collaboration, storage, and native protocol architecture.
- [`docs/design/wheat-local-cli.md`](./wheat-local-cli.md) — local CLI contract;
  the Phase 2 outstanding items for ignore, hooks, discovery, and configuration
  are the parents of this document.
- [`docs/design/wheat-agent-workspaces.md`](./wheat-agent-workspaces.md) — the
  agent workspace design that depends on workspace-wide capture.
- `crates/wheat-cli/` — current executable reference client.
- `crates/wheat-core/` — native model and deterministic application semantics.
- `crates/wheat-store/` — local immutable-object, packed-store, and publication
  implementations.
