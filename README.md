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

Storage records are BinSchema-generated binary only. The schemas live in
`crates/wheat-core/schemas/`; the generated code is committed under each crate's
`generated/` directory (`just gen-schema`, `just protocol-gen-schema`).

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

## Design docs

`docs/design/server-native-source-control-store.md` is the overall design. The
`docs/design/wheat-*.md` files cover the local CLI, local tooling, agent workspaces,
the native protocol, and storage efficiency. They were written while Wheat lived in
the gothab repository, so they still refer to gothab's forge.
