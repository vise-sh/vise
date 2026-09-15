set positional-arguments

# List available recipes.
default:
    @just --list

# ---------------------------------------------------------------------------
# Quality gates. `just check` mirrors what CI runs on every pull request.
# ---------------------------------------------------------------------------

# Run every check CI runs: formatting, lints, tests, OpenAPI spec.
check: fmt-check lint test sqlx-check spec-check

# Format all crates in place.
fmt:
    cargo fmt --all

# Fail if any file is not rustfmt-clean.
fmt-check:
    cargo fmt --all --check

# Clippy across the whole workspace; warnings are errors.
lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

# Run the workspace test suite.
test *ARGS:
    cargo test --workspace --all-features "$@"

# Check dependencies against the RustSec advisory database and the license
# allowlist in deny.toml (needs cargo-deny).
audit:
    cargo deny check advisories licenses

# Spell-check source and docs (needs typos: `cargo install typos-cli`).
typos:
    typos

# Line coverage for the whole workspace (needs cargo-llvm-cov).
coverage:
    cargo llvm-cov --workspace --all-features --lcov --output-path lcov.info

# Lint the shell scripts in scripts/ (needs shellcheck).
shellcheck:
    shellcheck scripts/*.sh

# ---------------------------------------------------------------------------
# sqlx offline query cache. The `.sqlx/` directory is committed so the
# workspace builds without a database (CI sets SQLX_OFFLINE=true).
# Re-run `just sqlx-prepare` whenever you change a query!/query_as! macro or a
# migration, and commit the result.
# ---------------------------------------------------------------------------

# Regenerate `.sqlx/` from the live database (needs DATABASE_URL + migrations).
sqlx-prepare:
    cargo sqlx prepare --workspace

# Build the vise-server container image for this machine's architecture.
docker-build:
    docker build -t vise-server .

# Fail if `.sqlx/` is stale relative to the code and live database schema.
sqlx-check:
    cargo sqlx prepare --check --workspace

# ---------------------------------------------------------------------------
# OpenAPI spec. `openapi/openapi.json` is committed and feeds the generated
# vise-client crate; regenerate it whenever the API surface changes.
# ---------------------------------------------------------------------------

# Regenerate openapi/openapi.json from the vise-api crate.
gen-spec:
    cargo run --example openapi -p vise-api

# Fail if the committed spec does not match what the code generates.
spec-check: gen-spec
    git diff --exit-code -- openapi/openapi.json

# ---------------------------------------------------------------------------
# Local development.
# ---------------------------------------------------------------------------

db-up:
    docker compose up -d postgres

db-down:
    docker compose down

db-reset:
    docker compose down -v
    docker compose up -d postgres

# Apply migrations with sqlx-cli. vise-server also applies them itself on
# startup; this is for the query macros, sqlx-prepare and tests, which need
# the schema before the server runs.
db-migrate:
    sqlx migrate run --source crates/vise-core/migrations

run-server:
    cargo watch -w bins -w crates -x 'run -p vise-server'

run-host:
    cargo watch -w bins -w crates -x 'run -p vise-host'

run-cli *ARGS:
    cargo run -p vise-cli -- "$@"
