# vise

bins:
- vise-cli - used to interact with the vise API
- vise-server - the HTTP server that serves the vise API
- vise-host - pulls sessions from the server and runs the agent

crates:
- vise-api - the API crate
- vise-client - automatically generates a client for the vise API
- vise-core - domain model, repositories, and pure logic

## Development

```sh
just db-up            # start Postgres (docker compose)
just db-migrate       # apply crates/vise-core/migrations
just run-server
just check            # fmt, clippy, tests, OpenAPI drift, sqlx cache drift
```

Pre-1.0, schema changes are folded into `0001_create_sessions.sql`; after
editing it run `just db-reset && just db-migrate`.

`sqlx` query macros are checked against `DATABASE_URL` when set and against
the committed `.sqlx` cache otherwise. After changing any `query!` run
`just sqlx-prepare` and commit the result.

Integration tests under `crates/vise-api/tests` need `DATABASE_URL`; each test
creates and drops its own `vise_test_*` database. Without `DATABASE_URL` they
print a skip notice and pass.

## Server configuration

| Variable | Purpose |
|---|---|
| `DATABASE_URL` | Postgres connection string (required) |
| `VISE_GITHUB_APP_ID`, `VISE_GITHUB_APP_PRIVATE_KEY_PATH` | GitHub App used to mint installation tokens for `github_repo` sessions and for PR tracking |
| `VISE_GITHUB_API_BASE` | GitHub API base URL (default `https://api.github.com`) |
| `VISE_PR_POLL_INTERVAL_SECS` | PR tracking poll interval (default `60`) |

The GitHub App needs these repository permissions:

- **Contents: read & write** — hosts clone and push
- **Pull requests: read & write** — agents open PRs; the server reads reviews
- **Checks: read** — the server reads check runs for PR tracking

## PR tracking and follow-up sessions

When a session finishes with `outcome.kind == "pr_opened"`, the server keeps
polling that PR until it merges or closes:

- `session.pr_status` is the snapshot: `state` (`review_pending`,
  `changes_requested`, `approved`, `merged`, `closed`, `sync_error`) and
  `checks` (`pending`, `passing`, `failing`, or absent when the head commit
  has no check runs). `vise sessions get` / `ls` include it.
- Transitions are appended to the session's event stream as
  `{"type": "pr_state_changed", "from": ..., "to": ...}` and
  `{"type": "checks_state_changed", ...}` — only when something changed.
  `vise sessions watch <id>` on a completed session tails these until the PR
  reaches a terminal state.
- `state` is derived, not raw: the latest decisive review per reviewer wins,
  any outstanding "changes requested" beats approvals, and approvals only
  count against the current head commit. Raw review comments are never stored.
- Repeated 401/403/404 responses mark the snapshot `sync_error` instead of
  letting it go stale; a later successful poll clears it.

To act on review feedback:

```sh
vise sessions follow-up <session-id> --instructions "keep the public API stable" --watch
```

This creates a new session with `parent_session_id` set, the same agent
config (unless overridden via the API), the same repo, and the PR's head
branch as `base_branch`. Its input is composed on the server from the PR's
current review threads (with file/line context) and failing check names.
The follow-up's outcome is `pr_updated`; tracking stays with the root
session that opened the PR.
