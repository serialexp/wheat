# Wheat

A Git-independent source-control prototype. Changes have stable IDs and are stacks
of immutable revisions made of guarded edits. Views are composed deterministically,
and conflicts are stored as data instead of blocking work. Git stays reachable
through import and push.

## Crates

| crate | what it is |
|---|---|
| `wheat-core` | Git-independent primitives: changes, revisions, guarded edits, views, applications, conflicts. The reference model. |
| `wheat-store` | Local content-addressed, copy-on-write storage and the persisted mutator. |
| `wheat-cli` | The `wheat` command: workspaces, `add`, `change`, `stack`, `diff`, `materialize`, `repack`, ... |
| `wheat-git` | Git import and push. |
| `wheat-ignore` | `.gitignore`-compatible ignore rules for workspaces. |
| `wheat-protocol` | Native wire protocol messages and framing. |
| `wheat-server` | Standalone native protocol server. |
| `wheat-bench` | Benchmark profiles, uploaded to Gothab. |
| `wheat-trace` | Opt-in span timing (`WHEAT_TRACE`, `WHEAT_TRACE_SUMMARY`) for the binaries. |

Storage records are BinSchema-generated binary only. The schemas live in
`crates/wheat-core/schemas/`; the generated code is committed under each crate's
`generated/` directory (`just gen-schema`, `just protocol-gen-schema`).

## Install

Release builds for Linux (x86_64 and aarch64) install `wheat` and `wheat-server` into
`~/.local/bin`:

```sh
curl -fsSL https://raw.githubusercontent.com/serialexp/wheat/main/install.sh | sh
```

On glibc 2.39 or newer (Ubuntu 24.04 and later) it installs the glibc build; on an
older glibc or a musl system such as Alpine, a static musl build. `WHEAT_LIBC=gnu` or
`musl` picks one, `WHEAT_VERSION=v0.2.0` a release, `WHEAT_INSTALL_DIR` another
directory. The CLI runs only on Linux: on macOS and Windows it refuses every command.

Releases come from `.github/workflows/release.yml`: push a tag `v<version>` matching
the workspace version in `Cargo.toml`, and it builds Linux, macOS and Windows
archives and publishes them with `SHA256SUMS`. `ci.yml` lints, tests and type-checks
for Windows on every push and pull request.

## Build and test

```sh
just check
just test          # cargo nextest (each test in its own process) + doctests
just clippy
just install       # installs `wheat` into ~/.cargo/bin
```

Tests never see your personal git config: `.cargo/config.toml` points git at an
empty global config, and `tests_see_no_git_config` in wheat-git fails if that is
bypassed.

## Where the time goes

Any `wheat` command can report its own timing:

- `WHEAT_TRACE_SUMMARY=1 wheat ...` prints a tree of spans (total ms, own ms, count)
  to stderr when the command ends.
- `WHEAT_TRACE=trace.json wheat ...` writes a Chrome trace event timeline. Open it in
  [Perfetto](https://ui.perfetto.dev) or `chrome://tracing`.

`wheat-server` honours the same variables and reports when stopped with Ctrl-C. Under
`wheat-bench quick`, each case process records its own spans, which covers the
in-process server of the remote cases; the `wheat` commands a case runs record none.
Use it with one `--case`, since every case writes the same `WHEAT_TRACE` file.

## Benchmarks

`wheat-bench` runs two profiles against release binaries:

- `wheat-quick` (`just bench`): focused scale cases, such as `add` over 20,000
  files, placing an edit under a 500-change stack, and content repack planning.
  It takes a few minutes. `just bench --case deep_stack` runs one case.
- `wheat-gothab-workflow` (`just bench-corpus ~/Projects/gothab`): the whole CLI
  workflow against a full Git clone. It takes about 55 minutes on the Gothab
  repository.

Each run writes `profile.json` (a Gothab profile report) and saves
`bench/results/<profile>/<stamp>-<rev>.tsv`. Commit that file. To see what changed,
run `just bench-compare OLD.tsv NEW.tsv`. Only compare runs from the same machine;
the command warns when the CPU or case sizes differ.

On Gothab, `gt profile wheat-quick` (or `wheat-gothab-workflow`) runs the profile on
a pinned runner and uploads it. Results show under the repository's performance page.
`.gothab/profiles.yml` names the workflows.

## Design docs

`docs/design/server-native-source-control-store.md` is the overall design. The
`docs/design/wheat-*.md` files cover the local CLI, local tooling, agent workspaces,
the native protocol, and storage efficiency. They were written while Wheat lived in
the gothab repository, so they still refer to gothab's forge.
