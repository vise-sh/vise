# vise

[![CI](https://github.com/vise-sh/vise/actions/workflows/ci.yml/badge.svg)](https://github.com/vise-sh/vise/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/vise-sh/vise/graph/badge.svg)](https://codecov.io/gh/vise-sh/vise)
[![crates.io](https://img.shields.io/crates/v/vise-cli.svg)](https://crates.io/crates/vise-cli)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

vise runs coding-agent sessions against your repositories on hosts you control.
You describe a task, the server schedules it onto an enrolled host, the host
runs an agent harness (such as Claude Code) in an isolated workspace using the
[Agent Client Protocol](https://agentclientprotocol.com), and the result comes
back as a pull request, a pushed branch, or a live event stream you can tail
from the CLI. The API is documented with OpenAPI and the Rust client is
generated from that spec, so the CLI, the host and any integration you write
all speak the same contract.

## Quickstart

You need Rust (stable), Docker and [just](https://github.com/casey/just).

```sh
git clone https://github.com/vise-sh/vise && cd vise
cp .env.example .env          # DATABASE_URL for the server and sqlx
just db-up && just db-migrate # Postgres 17 in Docker, then apply migrations
cargo run -p vise-server      # API on http://localhost:3000 (Swagger UI at /docs)
```

In a second terminal, enroll a host and start it with the token that is printed:

```sh
cargo run -p vise-cli -- hosts create laptop
VISE_HOST_TOKEN=<token> cargo run -p vise-host
```

Then create a session and watch it run:

```sh
cargo run -p vise-cli -- sessions create "add a --json flag to the ls command" \
  --repo your-org/your-repo --watch
```

Use `--harness echo` to try the flow without a real agent, and
`sessions ls` / `sessions events <id>` to inspect what happened. Prebuilt
`vise-cli` archives for macOS (arm64) and Linux (x86_64, arm64) are attached to
every [GitHub release](https://github.com/vise-sh/vise/releases).

## PR tracking and follow-up sessions

A session that ends by opening a pull request does not stop being useful
there. The server keeps polling the PR (every 60 seconds by default, see
`VISE_PR_POLL_INTERVAL_SECS`) until it merges or closes, and records a derived
snapshot on the session:

- `pr_status.state`: `review_pending`, `changes_requested`, `approved`,
  `merged`, `closed`, or `sync_error` when the PR became unreadable. The state
  is reduced from GitHub's review list (latest review per reviewer wins, an
  outstanding request for changes beats approvals, approvals on an older
  commit are stale); raw review comments are never stored.
- `pr_status.checks`: `pending`, `passing` or `failing`, from the check runs
  on the head commit.

Every transition is appended to the session's event stream as a
`pr_state_changed` or `checks_state_changed` event, so `sessions watch <id>`
on a finished session tails the PR until it merges or closes, and
`sessions ls` / `sessions get` show the current state.

When a reviewer asks for changes, spawn a follow-up:

```sh
cargo run -p vise-cli -- sessions follow-up <session-id> \
  --instructions "keep the public API stable" --watch
```

The follow-up inherits the parent's agent configuration, is checked out on the
PR's head branch so its pushes update the same PR, and receives the current
review threads (with file and line context) and failing check names in its
input, composed server-side at creation time. Its outcome is `pr_updated`;
tracking stays with the session that opened the PR, however many follow-ups
chain off it.

The tracker and the follow-up endpoint reuse the server's GitHub App
credential, which needs the *Pull requests: read* and *Checks: read*
permissions in addition to the *Contents: read/write* that hosts need to push.

## Architecture

The workspace is split into binaries you run and crates they share.

| Path | Kind | What it is |
|------|------|------------|
| `bins/vise-server` | binary | HTTP API, session scheduler, lease sweeper and PR tracker, backed by Postgres |
| `bins/vise-host` | binary | Runs on a machine you enroll; claims sessions and drives the agent harness |
| `bins/vise-cli` | binary | `vise` command-line client for sessions and hosts |
| `crates/vise-api` | library | axum routes, request/response types, OpenAPI document |
| `crates/vise-core` | library | Domain model, session state machine, sqlx repositories and migrations |
| `crates/vise-client` | library | Rust client generated at build time from `openapi/openapi.json` |

Longer design notes live in [`docs/`](docs/).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, conventions and the checks
CI runs. The short version is `just check`. Bugs and feature requests go
through [GitHub issues](https://github.com/vise-sh/vise/issues); please read
[SECURITY.md](SECURITY.md) before reporting a vulnerability and
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) before participating.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
