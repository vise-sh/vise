set positional-arguments

# Everything CI expects: formatting, lints, tests, and no drift in the
# generated OpenAPI spec or the sqlx offline query cache.
# Requires DATABASE_URL (integration tests create throwaway databases on it).
check: fmt-check lint test spec-check sqlx-check

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

gen-spec:
    cargo run --example openapi -p vise-api

# Fails when openapi/openapi.json is stale relative to the route definitions.
spec-check: gen-spec
    git diff --exit-code -- openapi/openapi.json

# Regenerate .sqlx so the workspace compiles without a live database.
sqlx-prepare:
    cargo sqlx prepare --workspace -- --all-targets

sqlx-check:
    cargo sqlx prepare --workspace --check -- --all-targets

db-up:
    docker compose up -d postgres

db-down:
    docker compose down

db-reset:
    docker compose down -v
    docker compose up -d postgres

db-migrate:
    sqlx migrate run --source crates/vise-core/migrations

run-server:
    cargo watch -w bins -w crates -x 'run -p vise-server'

run-host:
    cargo watch -w bins -w crates -x 'run -p vise-host'

run-cli *ARGS:
    cargo run -p vise-cli -- "$@"
