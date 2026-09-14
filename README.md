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

## Install

One line stands up the whole stack on a macOS (Apple Silicon) or Linux
(x86_64, arm64) machine that has Docker and git:

```sh
curl -fsSL https://vise.sh/install | sh
```

`vise.sh/install` serves [`scripts/install.sh`](scripts/install.sh) from this
repository. Without sudo, it:

1. checks for `docker` (with the compose plugin) and `git`, and warns if
   `claude` (Claude Code) is missing: the `claude-code` harness needs it, the
   `echo` harness does not;
2. asks for a GitHub personal access token (or reads `VISE_GITHUB_PAT`) and
   writes `~/.vise/.env` plus a `~/.vise/docker-compose.yml` that runs Postgres
   and `ghcr.io/vise-sh/vise-server`;
3. runs `docker compose up -d` and waits for the API on `http://localhost:3000`;
4. downloads the `vise-cli` and `vise-host` archives from the latest
   [GitHub release](https://github.com/vise-sh/vise/releases) into
   `~/.vise/bin` (add it to your `PATH`);
5. enrolls this machine as a host and starts `vise-host` in the background.

It is safe to re-run: an existing `~/.vise/.env` is kept unless you say
otherwise, and the binaries and server image are upgraded in place. Set
`VISE_VERSION=v0.2.0` to pin a release, `VISE_PORT` to move the API, or
`VISE_HOME` to install somewhere other than `~/.vise`; the header of the script
lists every override. Then:

```sh
vise sessions create "add a --json flag to the ls command" --repo owner/repo --watch
```

The host on this machine is a plain background process, managed with
`vise host`:

```sh
vise host status      # running / not running (exit 1 when stopped)
vise host logs -f     # tail ~/.vise/logs/host.log
vise host stop        # SIGTERM via ~/.vise/host.pid
vise host start       # reads VISE_HOST_TOKEN and VISE_URL from ~/.vise/.env
```

`vise host start -- --keep-workspaces` passes extra flags through to
`vise-host`. Server logs are at `docker compose -f ~/.vise/docker-compose.yml logs -f`.

### Uninstall

```sh
vise host stop
docker compose -f ~/.vise/docker-compose.yml down -v   # containers and the Postgres volume
rm -rf ~/.vise
```

and remove the `~/.vise/bin` line from your shell profile.

## Quickstart from source

Every release ships the server as a container image,
`ghcr.io/vise-sh/vise-server` (linux/amd64 and linux/arm64, tagged with the
version and `latest`), plus `vise-cli` and `vise-host` archives for macOS
(arm64) and Linux (x86_64, arm64) on the
[releases page](https://github.com/vise-sh/vise/releases). With those you
need Docker and nothing else; the server applies its own database migrations
on startup.

```sh
git clone https://github.com/vise-sh/vise && cd vise
cp .env.example .env     # set VISE_GITHUB_PAT (or the App settings, see below)
docker compose up -d     # Postgres 17 + vise-server on http://localhost:3000
```

Swagger UI is at `/docs`. While the repository is private, the image is too:
`docker login ghcr.io` with a token that has `read:packages` first. With a
GitHub App instead of a PAT, add the overlay that mounts the private key:
`docker compose -f docker-compose.yml -f docker-compose.github-app.yml up -d`.

To run the server from source instead, you also need Rust (stable) and
[just](https://github.com/casey/just):

```sh
just db-up               # Postgres only
cargo run -p vise-server # applies migrations, then serves on :3000
```

Enroll a host and start it with the token that is printed (use the release
binaries or `cargo run -p ...` interchangeably):

```sh
vise-cli hosts create laptop
VISE_HOST_TOKEN=<token> vise-host
```

(`cargo run -p vise-cli -- host start --bin target/debug/vise-host` runs the
same thing in the background, with the token taken from `VISE_HOST_TOKEN` or
`~/.vise/.env`.)

Then create a session and watch it run:

```sh
vise-cli sessions create "add a --json flag to the ls command" \
  --repo your-org/your-repo --watch
```

Use `--harness echo` to try the flow without a real agent, and
`sessions ls` / `sessions events <id>` to inspect what happened.

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

The tracker and the follow-up endpoint reuse the server's GitHub credential
(App or PAT, see below), which needs the *Pull requests: read* and
*Checks: read* permissions in addition to the *Contents: read/write* that
hosts need to push.

## GitHub authentication

`github_repo` sessions need the server to authenticate to GitHub, both to hand
hosts a token for cloning and pushing and to read pull requests for tracking
and follow-ups. Two options are supported:

- **GitHub App** (recommended for organizations): set `VISE_GITHUB_APP_ID`
  and `VISE_GITHUB_APP_PRIVATE_KEY_PATH`. The server mints a short-lived
  installation token scoped to the session's repository for every session.
- **Personal access token**: set `VISE_GITHUB_PAT` when the App is not
  installed. The same token is handed to every session and used for all
  server-side reads. Use a [fine-grained PAT](https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/managing-your-personal-access-tokens#creating-a-fine-grained-personal-access-token)
  restricted to the repositories vise works on, with *Contents: read/write*,
  *Pull requests: read/write* and *Checks: read* (the same permissions the App
  needs).

Both modes get identical behaviour: hosts obtain the credential through the
same endpoint, and PR tracking and follow-up sessions work the same way. If
both are configured, the App wins and the PAT is ignored (the server logs
this at startup). With neither, `github_repo` sessions fail and PR tracking is
disabled.

## Architecture

The workspace is split into binaries you run and crates they share.

| Path | Kind | What it is |
|------|------|------------|
| `bins/vise-server` | binary | HTTP API, session scheduler, lease sweeper and PR tracker, backed by Postgres |
| `bins/vise-host` | binary | Runs on a machine you enroll; claims sessions and drives the agent harness |
| `bins/vise-cli` | binary | `vise` command-line client for sessions and hosts, and the `vise host` supervisor for the local host process |
| `crates/vise-api` | library | axum routes, request/response types, OpenAPI document |
| `crates/vise-core` | library | Domain model, session state machine, sqlx repositories and migrations (embedded, applied by the server on startup) |
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
