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

# Run the whole suite (nextest runs each test in its own process), including the
# examples' unit tests, plus doctests, which nextest does not run.
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

# Install the wheat CLI to ~/.cargo/bin/.
install:
    cargo install --path crates/wheat-cli --bin wheat --locked --force
