---
title: Concepts
description: Sessions, hosts, harnesses, environments, outcomes and PR tracking, as implemented in v0.1.0.
sidebar:
  order: 4
---

This page defines the vocabulary vise uses across the CLI, the API and the
event stream. Everything here reflects what v0.1.0 does, not a roadmap.

## Session

A session is one run of an agent against one prompt. It is created with a
`POST /sessions` (or `vise sessions create`) carrying three things:

- **`input`**: the prompt.
- **`agent`**: the harness to run, an optional model hint, optional
  instructions prepended to the prompt, and a (currently unused) list of MCP
  servers.
- **`environment`**: where the agent works. See [Environments](#environments).

Session IDs look like `ses_01k5c3ab7ke8xz7t2m9qn4rw6d`: a `ses_` prefix and
a 26-character, time-ordered identifier, so IDs sort by creation time.

### Lifecycle

```text
pending ──claim──▶ running ──finish──▶ completed
                      │                 failed
                      │                 cancelled
                      └── lease expires ──▶ failed ("lease expired")
```

| Status | Meaning |
|--------|---------|
| `pending` | Queued, waiting for a host to claim it |
| `running` | Claimed by a host, which holds a lease on it |
| `completed` | The agent finished its turn; `stop_reason` says how (for example `end_turn`) |
| `failed` | Workspace preparation or the agent run failed (`error` has the message), or the host's lease expired |
| `cancelled` | Cancellation was requested and the host stopped the agent |

Scheduling is first-in, first-out: a host's claim takes the oldest `pending`
session regardless of harness or environment. `POST /sessions/{id}/cancel`
cancels a `pending` session immediately; on a `running` one it sets
`cancel_requested`, and the host stops the agent when it sees the flag on its
next heartbeat. Terminal sessions cannot be cancelled.

### Events

Everything that happens in a session is appended to its event log with a
monotonically increasing `seq`. `GET /sessions/{id}/events?after_seq=N` pages
through it, and `GET /sessions/{id}/events/stream` tails it over Server-Sent
Events, closing with a `done` event once the session is terminal (and, for a
session that opened a PR, once the PR has merged or closed).

Three kinds of payload appear in the log:

- **ACP session updates** from the harness: `agent_message_chunk`,
  `agent_thought_chunk`, `tool_call`, `tool_call_update` and `plan`, wrapped as
  `{ "sessionId": ..., "update": { "sessionUpdate": ..., ... } }`.
- **Permission audit records**: whenever the agent asks for permission, the
  host auto-approves the first offered option and records both the request
  and the chosen option as an event. `vise sessions watch` renders these as
  `[permission auto-approved]`.
- **PR tracking transitions** written by the server after the session
  finishes: `pr_state_changed` and `checks_state_changed`, each with `from`
  and `to`.

## Host

A host is a machine you have enrolled to run sessions. Enrolling
(`vise hosts create <name>`, or `POST /hosts`) returns a bearer token with a
`vhost_` prefix exactly once; the server stores only its SHA-256 hash.
`vise-host` presents that token on every request and the server records the
host's `last_seen_at`.

`vise-host` is a single loop: claim a session, run it, repeat, sleeping two
seconds (`--poll-interval`) when there is nothing to claim. It runs natively
rather than in a container because it spawns the agent installed on that
machine.

### Leases and heartbeats

A claim gives the host a 60-second lease on the session. While the session
runs, the host heartbeats every 20 seconds to extend the lease, and the
heartbeat response carries the `cancel_requested` flag. The server sweeps leases every 15
seconds; a session whose lease lapses (because the host crashed or lost
connectivity) is marked `failed` with the error `lease expired`, so no session
stays `running` forever.

### Workspaces

Every session gets a directory under the host's temp directory at
`vise-sessions/<session-id>`. For `github_repo` sessions the repository is
cloned inside it; for `self_hosted` sessions it is an empty `workspace`
folder. Workspaces are deleted when the session finishes unless the outcome is
`uncommitted_changes` or the host runs with `--keep-workspaces`. Kept
workspaces are marked with a `.keep` file so the host's startup cleanup leaves
them alone.

## Harness

A harness is the agent runtime the host launches. The harness-to-command
mapping lives in the host, not the server; v0.1.0 hardcodes two:

| Harness | What runs | Needs |
|---------|-----------|-------|
| `claude-code` (default) | Claude Code over ACP, via `npx -y @agentclientprotocol/claude-agent-acp@latest` | `claude` installed and authenticated on the host, plus Node for `npx` |
| `echo` | A built-in fake that echoes the prompt back as `agent_message_chunk` events | Nothing |

The host talks to the agent over the
[Agent Client Protocol](https://agentclientprotocol.com): it initializes the
agent, opens an ACP session rooted at the workspace, sends one prompt turn
(instructions, a blank line, then the input), forwards every session update
as an event, and treats the agent's stop reason as the session's
`stop_reason`. For `github_repo` sessions the GitHub credential is passed to
the agent process as `GH_TOKEN` and `GITHUB_TOKEN`, so `git push` and `gh pr
create` work inside the checkout.

## Environments

The environment says where the agent works:

- **`self_hosted`**: an empty scratch directory on the host. No clone, no
  credentials, no outcome detection. Useful for smoke tests with `echo`.
- **`github_repo`**: `repo` is required as `owner/name`; `base_branch` is
  optional and defaults to the repository's default branch. The host asks the
  server for a GitHub credential, clones the repository, and refreshes the
  credential while the session runs. The CLI switches to this environment
  whenever `--repo` is given.

### GitHub credentials

Hosts never hold a long-lived GitHub secret of their own. They call
`POST /hosts/sessions/{id}/credentials` and the server answers with whichever
credential it is configured with:

- With a **GitHub App** (`VISE_GITHUB_APP_ID` and
  `VISE_GITHUB_APP_PRIVATE_KEY_PATH`), the server mints a short-lived
  installation token scoped to the session's repository.
- With a **personal access token** (`VISE_GITHUB_PAT`), the same token is
  handed to every session.

The server uses the same credential for its own PR tracking reads. If both
are set, the App wins.

## Outcomes

When a `github_repo` session's agent run succeeds, the host inspects the
checkout and reports one of five outcomes on `POST /hosts/sessions/{id}/finish`:

| Outcome | How it is detected |
|---------|--------------------|
| `no_changes` | Still on the base branch at the base commit, working tree clean |
| `uncommitted_changes` | Dirty working tree, or commits that were never pushed (locally-only work). The workspace is kept |
| `pushed_no_pr` | The current branch is pushed and matches the remote, but no open PR has it as head |
| `pr_opened` | The branch is pushed and an open PR has it as head; `pr_url` is set |
| `pr_updated` | A follow-up session pushed to the head branch of the PR it was addressing |

`self_hosted` sessions have no outcome.

## PR tracking

A session whose outcome is `pr_opened` becomes a tracking root. The server's
poller visits it every 60 seconds (`VISE_PR_POLL_INTERVAL_SECS`) until the PR
merges or closes, and records a derived snapshot on the session:

- **`pr_status.state`**: `review_pending`, `changes_requested`, `approved`,
  `merged`, `closed`, or `sync_error` when the PR could not be read
  (persistent 403 or 404; retried every tick and cleared once a fetch
  succeeds). The state is reduced from GitHub's review list: the latest
  review per reviewer wins, an outstanding request for changes beats
  approvals, and approvals on an older commit are stale. Raw review comments
  are never stored.
- **`pr_status.checks`**: `pending`, `passing` or `failing`, from the check
  runs on the head commit. Absent when the head commit has no check runs.
- **`pr_status.last_synced_at`**: when the snapshot was last refreshed.

Every change is appended to the root session's events as `pr_state_changed`
or `checks_state_changed`, which is what lets `vise sessions watch` on a
finished session keep tailing until the PR is merged or closed.

## Follow-up sessions

`POST /sessions/{id}/follow-up` (or `vise sessions follow-up`) creates a new
session that addresses review feedback on an existing session's PR:

- It inherits the parent's `agent` configuration unless the request overrides
  it.
- Its environment is the same repository with `base_branch` set to the PR's
  head branch, so its pushes update the same PR.
- Its input is composed server-side at creation time: the current review
  threads with file and line context, the names of failing checks, and any
  extra `instructions` you pass.
- `parent_session_id` points at the session it was spawned from. Follow-ups
  can chain, but PR tracking always lives on the root session that opened the
  PR, resolved by walking the parent chain.

## Server

`vise-server` is one process with four jobs, backed by Postgres:

1. **The HTTP API** on port 3000: the `/sessions` routes the CLI uses and the
   `/hosts` routes hosts use, with Swagger UI at `/docs`. The
   `openapi/openapi.json` document is committed and the Rust client is
   generated from it.
2. **Scheduling**: claims hand out `pending` sessions FIFO with row locking,
   so concurrent hosts never claim the same session.
3. **The lease sweeper**: every 15 seconds, fails sessions whose lease lapsed.
4. **The PR tracker**: polls PRs as described above.

It applies its own database migrations on startup, so a release image or
binary needs nothing beyond `DATABASE_URL` and a GitHub credential.

## API surface

| Method and path | Who calls it | Purpose |
|-----------------|--------------|---------|
| `GET /sessions` | CLI | List sessions |
| `POST /sessions` | CLI | Create a session |
| `GET /sessions/{id}` | CLI, host | Fetch one session |
| `GET /sessions/{id}/events` | CLI | Page through events |
| `GET /sessions/{id}/events/stream` | CLI | Tail events over SSE (not in the OpenAPI document) |
| `POST /sessions/{id}/cancel` | CLI | Request cancellation |
| `POST /sessions/{id}/follow-up` | CLI | Spawn a follow-up session |
| `POST /hosts` | CLI | Enroll a host; returns the token once |
| `GET /hosts` | CLI | List hosts |
| `POST /hosts/claim` | host | Claim the next pending session |
| `POST /hosts/sessions/{id}/heartbeat` | host | Extend the lease; learn of cancellation |
| `POST /hosts/sessions/{id}/events` | host | Report a batch of events |
| `POST /hosts/sessions/{id}/credentials` | host | Obtain a GitHub credential |
| `POST /hosts/sessions/{id}/finish` | host | Record the terminal status and outcome |

The `/hosts/claim` and `/hosts/sessions/*` routes require a host bearer token.
The `/sessions` routes are unauthenticated in v0.1.0; the installer binds the
API to `127.0.0.1` only, and you should keep it that way unless you put
something in front of it.
