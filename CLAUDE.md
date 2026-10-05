# Wheat

A Git-independent source-control prototype in Rust. See `README.md` for the crates.
It used to live in the gothab repository and was moved here on 2026-09-25 with its
history. It must never depend on a gothab crate.

## Storage format

Storage records are **BinSchema-generated binary only** (see
`crates/wheat-core/CLAUDE.md`). No JSON, serde or hand-rolled encodings for anything
persisted. Schemas: `crates/wheat-core/schemas/*.bs.json5`. Regenerate with
`just gen-schema` / `just protocol-gen-schema` (needs `~/Projects/binschema` at the
revision pinned in the scripts). Unreleased formats are edited in place: no V2
structs, no version bumps.

## Building and testing

- `just check`, `just clippy`, `just fmt-check`, `just test` (nextest plus doctests).
- `just check-windows`: everything must build for Windows, though workspace file
  operations only run on Linux. New operating-system calls in wheat-cli go through
  `crates/wheat-cli/src/sys.rs`, not `libc` or `cfg` at the call site.
- Use `cargo nextest`, not plain `cargo test`: several suites rely on each test having
  its own process.
- Build with `-j 2` / `--build-jobs 2` on Bart's machine.
- Tests never see personal git config (`.cargo/config.toml` `[env]`); write tests
  against git's built-in defaults.
- Anything more than a one-line change ships with a test. Leave the suite green.
- Performance is part of correctness: work that scales with repository size needs a
  scale test or benchmark. Benchmarks are cases in `crates/wheat-bench` (`just bench`),
  not ignored tests. A new case gets a smoke size so `just test` keeps it running, and
  a contract rule when it has a budget. Run the case in release before calling the work
  done, and commit the results file it saves under `bench/results/`.

## Finding where time goes

Don't add ad-hoc timing prints. Put a `#[tracing::instrument(level = "debug",
skip_all)]` (or a `debug_span!`) on the step, then run with
`WHEAT_TRACE_SUMMARY=1` for a span tree or `WHEAT_TRACE=trace.json` for a Perfetto
timeline. Only spans whose target starts with `wheat` are recorded.

## Commits

Conventional Commits: `type(scope): description`, lowercase, imperative, no trailing
period. Scopes are the crate areas: `core`, `store`, `cli`, `git`, `server`,
`protocol`, `ignore`, or `wheat` for changes across several.

## TODO bookkeeping

Open work is in `TODO.md`. When an item is done, move it to `TODO_DONE.md` under the
same heading in the same commit.
