# Wheat

A Git-independent source-control prototype in Rust. See `README.md` for the crates.
It used to live in the gothab repository and was moved here on 2026-09-25 with its
history. It must never depend on a gothab crate.

## Storage format

What Wheat persists falls in three kinds (see `crates/wheat-core/CLAUDE.md`):

- **Durable system state** is a **BinSchema-generated binary** record or a SQLite
  database (the store locator, the catalog, the git journal). No JSON, serde or
  hand-rolled encodings for anything the system writes and reads back later.
- **Files people edit** are JSON: `config.json` (user and store) and the tracked
  `.wheat.json` at a workspace's root (`repository_config.rs`).
- **Temporary files** that never outlive the operation that wrote them (for example
  `materialization.json`, `rewrite-transaction.json`) may be JSON.

Schemas: `crates/wheat-core/schemas/*.bs.json5`. Regenerate with
`just gen-schema` / `just protocol-gen-schema` (needs `~/Projects/binschema` at the
revision pinned in the scripts).

## Building and testing

- `just check`, `just clippy`, `just fmt-check`, `just test` (nextest plus doctests).
- `just check-windows`: everything must build for Windows, though workspace file
  operations only run on Linux. New operating-system calls in wheat-client go through
  `crates/wheat-client/src/sys.rs`, not `libc` or `cfg` at the call site.
- Use `cargo nextest`, not plain `cargo test`: several suites rely on each test having
  its own process.
- Each crate builds its integration tests as one binary, `integration`
  (`tests/harnesses/integration.rs`, with `autotests = false`). A new test file goes
  under `tests/` and gets a `#[path]` module line in that harness; never a new
  `[[test]]`. Every test binary links most of Wheat, and one per file is how
  `target/` fills the disk. `crates/wheat-core/tests/test_binaries.rs` enforces it.
- Build with `-j 2` / `--build-jobs 2` on Bart's machine.
- Tests never see personal git config (`.cargo/config.toml` `[env]`); write tests
  against git's built-in defaults.
- Anything more than a one-line change ships with a test. Leave the suite green.
- Performance is part of correctness: work that scales with repository size needs a
  scale test or benchmark. Benchmarks are cases in `crates/wheat-bench` (`just bench`),
  not ignored tests. A new case gets a smoke size so `just test` keeps it running, and
  a contract rule when it has a budget. Run the case in release before calling the work
  done, and commit the results file it saves under `bench/results/`.

## Reviews

Validate input at trust boundaries (network, disk, other processes), not between
Wheat's own crates. When a review asks for a check against a trusted internal caller,
say what failure it would catch before adding it. Performance contracts count work
(objects read, bytes written, requests) rather than time, so an agent copying a slow
pattern, like a linear scan where an index exists, fails a test.

## Finding where time goes

Don't add ad-hoc timing prints. Put a `#[tracing::instrument(level = "debug",
skip_all)]` (or a `debug_span!`) on the step, then run with
`WHEAT_TRACE_SUMMARY=1` for a span tree or `WHEAT_TRACE=trace.json` for a Perfetto
timeline. Only spans whose target starts with `wheat` are recorded.

## Desktop client design

The Claude Design project for the Wheat desktop client is
`0dfb95ac-9975-4817-99e6-a7cf09ceae27`; the comp is `Wheat Client.dc.html`
(https://claude.ai/design/p/0dfb95ac-9975-4817-99e6-a7cf09ceae27?file=Wheat+Client.dc.html).
The brief it was made from is `docs/wheat-gui-design-brief.md`.

## Commits

Conventional Commits: `type(scope): description`, lowercase, imperative, no trailing
period. Scopes are the crate areas: `core`, `store`, `cli`, `git`, `server`,
`protocol`, `ignore`, `client`, `desktop`, or `wheat` for changes across several.

## TODO bookkeeping

Open work is in `TODO.md`. When an item is done, move it to `TODO_DONE.md` under the
same heading in the same commit.
