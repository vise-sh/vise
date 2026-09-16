---
title: Quickstart
description: Run a first session with the echo harness, then a real one against a GitHub repository.
sidebar:
  order: 3
---

This walkthrough assumes you have run the [installer](/getting-started/install/):
the server is on `http://localhost:3000`, a host is polling for work, and
`vise` is on your `PATH`.

:::note
The example output below is derived from the CLI's source, not captured from
a live run. IDs, timestamps and agent text will differ on your machine.
:::

## 1. Check the host is running

```sh
vise host status
```

```text
vise-host: running (pid 48213)
  log: /Users/you/.vise/logs/host.log
```

If it prints `vise-host: not running` (and exits 1), start it with
`vise host start`. You can also confirm the server sees it:

```sh
vise hosts ls
```

## 2. Run an echo session

The `echo` harness needs no agent and no credentials. It echoes your prompt
back as agent messages, which proves the server, host and CLI are wired up:

```sh
vise sessions create "hello from vise" --harness echo --watch
```

```text
created ses_01k5c3ab7ke8xz7t2m9qn4rw6d
Echo: hello from vise

── session completed ──

```

Without `--repo`, the session runs in a `self_hosted` environment: an empty
scratch directory on the host, no clone, no outcome detection.

`--watch` prints the session ID to stderr, then tails the event stream until
the session reaches a terminal state. Drop `--watch` and the command prints
the created session as JSON instead, so you can script against it.

## 3. Inspect what happened

List sessions:

```sh
vise sessions ls
```

```text
ID                               STATUS     OUTCOME              PR                 CHECKS   CREATED
ses_01k5c3ab7ke8xz7t2m9qn4rw6d   completed  -                    -                  -        2026-09-16T15:04:05Z
```

Get one session as JSON, or its raw event log:

```sh
vise sessions get ses_01k5c3ab7ke8xz7t2m9qn4rw6d
vise sessions events ses_01k5c3ab7ke8xz7t2m9qn4rw6d
```

Events are ordered by a `seq` number. `sessions events --after-seq N` and
`sessions watch --after-seq N` resume from a position; `0` replays the full
history.

## 4. Run a real session against a repository

With Claude Code installed on the host and a GitHub token configured, point a
session at a repository:

```sh
vise sessions create "add a --json flag to the ls command" --repo your-org/your-repo --watch
```

The host clones the repository with a short-lived credential from the server,
runs Claude Code in the checkout over ACP, and streams events back. `--watch`
renders agent text inline, one line per tool call, and a marker whenever the
agent's permission request was auto-approved:

```text
created ses_01k5c3g2p9x6ymv4qz8t1rhn0b

[tool] Read src/cli.rs
[tool] Edit src/cli.rs
I added a `--json` flag to the `ls` subcommand and covered it with a test.

[tool] Bash: git push -u origin vise/ls-json-flag

[tool] Bash: gh pr create ...
Opened a pull request with the change.

── session completed ──

PR: https://github.com/your-org/your-repo/pull/42
```

When the session finishes, the host inspects the checkout and reports one of
five outcomes; `--watch` prints a line for each:

| Outcome | Printed as |
|---------|------------|
| `pr_opened` | `PR: <url>` |
| `pr_updated` (follow-ups only) | `PR updated: <url>` |
| `pushed_no_pr` | `pushed branch <name> (no PR)` |
| `uncommitted_changes` | `warning: agent left uncommitted work; workspace kept on host` |
| `no_changes` | `no changes made` |

Useful flags on `sessions create`:

- `--base-branch <name>` bases the work on a branch other than the
  repository default.
- `--instructions "<text>"` prepends guidance to the prompt, separated from it
  by a blank line.
- `--model <hint>` passes a model hint to the harness.
- `--harness echo` runs the fake harness even with `--repo`.

## 5. Follow the PR to merge

A session that opened a PR stays useful. The server polls the PR every 60
seconds (`VISE_PR_POLL_INTERVAL_SECS`) until it merges or closes, recording
the review and check state on the session. `sessions ls` and `sessions get`
show the current snapshot:

```text
ID                               STATUS     OUTCOME              PR                 CHECKS   CREATED
ses_01k5c3g2p9x6ymv4qz8t1rhn0b   completed  pr_opened            review_pending     passing  2026-09-16T15:12:40Z
```

Running `sessions watch` on a finished session that opened a PR tails those
transitions until the PR reaches a terminal state:

```sh
vise sessions watch ses_01k5c3g2p9x6ymv4qz8t1rhn0b
```

```text

[checks] pending -> passing

[pr] review_pending -> approved

── pr merged ──

PR: https://github.com/your-org/your-repo/pull/42
PR https://github.com/your-org/your-repo/pull/42: merged, checks passing (synced 2026-09-16T16:02:11Z)
```

## 6. Address review feedback with a follow-up

When a reviewer requests changes, spawn a follow-up instead of starting over:

```sh
vise sessions follow-up ses_01k5c3g2p9x6ymv4qz8t1rhn0b \
  --instructions "keep the public API stable" --watch
```

```text
created follow-up ses_01k5c4t8nq2wxk6vzj3ym9r0hc (parent ses_01k5c3g2p9x6ymv4qz8t1rhn0b)
...
── session completed ──

PR updated: https://github.com/your-org/your-repo/pull/42
PR tracking continues on the root session (parent: ses_01k5c3g2p9x6ymv4qz8t1rhn0b)
```

The follow-up inherits the parent's agent configuration, is checked out on the
PR's head branch so its pushes update the same PR, and receives the current
review threads (with file and line context) and the names of failing checks in
its input, composed by the server at creation time. Its outcome is
`pr_updated`. PR tracking stays with the session that opened the PR, however
many follow-ups chain off it.

## 7. Cancel a session

```sh
vise sessions cancel ses_01k5c4t8nq2wxk6vzj3ym9r0hc
```

A session that is still `pending` is cancelled on the spot. A `running` one is
marked cancel-requested; the host notices on its next heartbeat (within 20
seconds), stops the agent, and finishes the session as `cancelled`.

## Where things live on the host

Each session runs in a workspace under the host's temp directory, at
`vise-sessions/<session-id>`. Workspaces are removed when the session
finishes, except when the outcome is `uncommitted_changes` (so the work is not
lost) or the host was started with `--keep-workspaces`. On startup the host
also removes leftover workspaces of sessions that have since finished.

## Next

[Concepts](/getting-started/concepts/) explains the vocabulary in depth:
sessions and their lifecycle, hosts and leases, harnesses, environments,
outcomes, and PR tracking.
