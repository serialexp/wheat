# Wheat task runner. `just --list` shows every recipe.

default:
    @just --list

# Type-check every crate and target.
check:
    cargo check --workspace --all-targets

# Lint every crate and target; warnings fail.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Verify formatting.
fmt-check:
    cargo fmt --all -- --check

# Run the whole suite (nextest runs each test in its own process), plus doctests,
# which nextest does not run.
test *ARGS:
    cargo nextest run --workspace --all-targets {{ARGS}}
    cargo test --workspace --doc

# Regenerate or verify Wheat's committed canonical-object wire types.
gen-schema:
    ./scripts/wheat-gen-schema

gen-schema-check:
    ./scripts/wheat-gen-schema --check

# Regenerate or verify Wheat's committed native-protocol wire types.
protocol-gen-schema:
    ./scripts/wheat-protocol-gen-schema

protocol-gen-schema-check:
    ./scripts/wheat-protocol-gen-schema --check

# Build the release binaries the benchmarks measure.
bench-build:
    cargo build --release -j 2 -p wheat-cli -p wheat-bench --bin wheat --bin wheat-bench

# Run the `wheat-quick` profile and save a results file under bench/results/.
# Extra arguments go to `wheat-bench quick`, e.g. `just bench --case deep_stack`.
bench *ARGS: bench-build
    target/release/wheat-bench quick {{ARGS}}

# Run the `wheat-gothab-workflow` profile against a full Git clone (about 55 minutes
# on the Gothab repository).
bench-corpus CORPUS *ARGS: bench-build
    target/release/wheat-bench corpus {{CORPUS}} {{ARGS}}

# Compare two results files, e.g. `just bench-compare bench/results/wheat-quick/{A,B}.tsv`.
bench-compare OLD NEW: bench-build
    target/release/wheat-bench compare {{OLD}} {{NEW}}

# Install the wheat CLI to ~/.cargo/bin/.
install:
    cargo install --path crates/wheat-cli --bin wheat --locked --force
