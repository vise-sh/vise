set positional-arguments

gen-spec:
    cargo run --example openapi -p vise-api

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
