# Contributing to vise

This document describes the tooling and conventions every change is expected
to follow. CI enforces all of it on pull requests, so running the same checks
locally before you push saves a round trip.

## Prerequisites

| Tool | Why | Install |
|------|-----|---------|
| Rust (stable) | Toolchain, pinned by `rust-toolchain.toml`; rustup installs `rustfmt` and `clippy` automatically | https://rustup.rs |
| [just](https://github.com/casey/just) | Task runner; every recipe below is `just <name>` | `cargo install just` |
| Docker | Local Postgres via `docker compose` | https://docs.docker.com/get-docker/ |
| [sqlx-cli](https://github.com/launchbadge/sqlx/tree/main/sqlx-cli) | Migrations and the offline query cache | `cargo install sqlx-cli --no-default-features --features postgres` |
| [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | Dependency advisory audit (optional locally) | `cargo install cargo-deny` |
| [cargo-watch](https://github.com/watchexec/cargo-watch) | `just run-server` / `just run-host` (optional) | `cargo install cargo-watch` |

## First-time setup

```sh
cp .env.example .env        # DATABASE_URL for the server, sqlx-cli and query macros
just db-up                  # start Postgres 17 in Docker
just db-migrate             # apply crates/vise-core/migrations
just check                  # confirm everything is green before you start
```

Optionally enable the pre-commit hook, which runs the fast formatting check:

```sh
git config core.hooksPath .githooks
```

## Before opening a pull request

Run the full gate. It is the same set of checks CI runs:

```sh
just check
```

That expands to:

| Recipe | What it enforces |
|--------|------------------|
| `just fmt-check` | Code is `rustfmt`-clean (`just fmt` fixes it) |
| `just lint` | `cargo clippy` on all targets and features with `-D warnings` |
| `just test` | `cargo test --workspace` |
| `just sqlx-check` | The committed `.sqlx/` query cache matches the code and DB schema |
| `just spec-check` | The committed `openapi/openapi.json` matches what `vise-api` generates |

`just audit` (cargo-deny advisories) also runs in CI but is not part of
`just check` because it needs network access and an extra tool.

## Formatting

We use `rustfmt` with default settings (`rustfmt.toml` only pins the edition).
Configure your editor to format on save, or run `just fmt`. Do not hand-format
around rustfmt; if the output looks bad, restructure the code instead.

## Linting

Clippy runs at its default lint level, and CI treats every warning as an
error. A few extra lints are enabled for the whole workspace in the
`[workspace.lints]` table of the root `Cargo.toml`; each crate opts in with
`[lints] workspace = true`, so add that block to any new crate.

Fix warnings rather than suppressing them. If a suppression is genuinely the
right call, scope it as narrowly as possible (a single item, not a module or
crate) and leave a one-line comment explaining why:

```rust
// Clippy wants `&[u8]`, but the FFI boundary requires an owned Vec.
#[allow(clippy::ptr_arg)]
fn send(buf: &Vec<u8>) { /* ... */ }
```

## Database queries and the sqlx offline cache

`vise-core` uses `sqlx::query!` / `query_as!`, which type-check SQL at compile
time. To keep the workspace buildable without a database (in CI and for
contributors who only touch the CLI or host), the results are cached in the
committed `.sqlx/` directory.

Whenever you add or edit a `query!` macro, or add a migration that changes a
column a query touches:

```sh
just db-migrate       # apply new migrations to your local DB
just sqlx-prepare     # regenerate .sqlx/
git add .sqlx
```

If you forget, `just sqlx-check` (and the `sqlx query cache up to date` CI
job) fails. With `DATABASE_URL` set, the macros validate against your live
database instead of the cache, so a stale cache will not show up as a local
build error; that is exactly why the check exists.

## OpenAPI spec and the generated client

`crates/vise-client` is generated at build time from `openapi/openapi.json`,
which is in turn generated from the `vise-api` crate. After changing any route,
request or response type, regenerate and commit the spec:

```sh
just gen-spec
git add openapi/openapi.json
```

`just spec-check` fails if the committed file is out of date.

## Dependencies

- Add shared dependencies to `[workspace.dependencies]` in the root
  `Cargo.toml` and reference them with `{ workspace = true }` so every crate
  uses the same version and feature set.
- Keep `Cargo.lock` committed and up to date.
- `just audit` checks the lockfile against the RustSec advisory database. If
  an advisory has no fix yet, add it to the `ignore` list in `deny.toml` with a
  comment and a link.

## Commits and pull requests

- Keep PRs focused; one logical change per PR is easier to review and revert.
- Write commit messages in the imperative mood with a short subject line and,
  when useful, a body that explains *why*.
- CI must be green before merge. If a check fails for a reason unrelated to
  your change, say so in the PR rather than working around it.

## Continuous integration

`.github/workflows/ci.yml` runs on every pull request and on pushes to `main`:

| Job | Command |
|-----|---------|
| rustfmt | `just fmt-check` |
| clippy | `just lint` |
| test | `just test` |
| openapi spec up to date | `just spec-check` |
| sqlx query cache up to date | `just db-migrate && just sqlx-check` against a Postgres 17 service |
| cargo deny advisories | `cargo deny check advisories` |

All jobs except `sqlx query cache up to date` build with `SQLX_OFFLINE=true`,
which is why the `.sqlx/` cache must be committed.
