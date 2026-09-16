---
title: Introduction
description: What vise is, what it does, and how the pieces fit together.
sidebar:
  order: 1
---

vise runs coding-agent sessions against your repositories on hosts you
control. You describe a task, the server schedules it onto an enrolled host,
the host runs an agent harness (such as Claude Code) in an isolated workspace
using the [Agent Client Protocol](https://agentclientprotocol.com), and the
result comes back as a pull request, a pushed branch, or a live event stream
you can tail from the CLI.

The whole stack is open source (MIT or Apache-2.0) and runs on your own
infrastructure: the server, the hosts, the agent and the model credentials all
stay with you. Only GitHub is outside it.

## How a session flows

1. **You create a session** with the `vise` CLI (or a `POST /sessions` call).
   It carries the prompt, the harness to run it with, and the environment:
   either a GitHub repository or a bare `self_hosted` workspace.
2. **The server queues it.** Sessions start `pending` and are handed out
   first-in, first-out to whichever host asks next.
3. **A host claims it.** `vise-host` polls the server every two seconds. On a
   claim the session becomes `running`, the host clones the repository with a
   short-lived GitHub credential issued by the server, and starts the harness
   in that checkout.
4. **Events stream back.** Everything the agent says and every tool call it
   makes is reported to the server as ordered events. `vise sessions watch`
   tails them live over Server-Sent Events.
5. **The host records the outcome.** When the agent finishes, the host inspects
   the checkout and reports whether a PR was opened, a branch was pushed,
   changes were left uncommitted, or nothing changed. The session ends as
   `completed`, `failed` or `cancelled`.
6. **The server tracks the PR.** For any session that opened a pull request,
   the server keeps polling GitHub until the PR merges or closes, appending
   review and check state transitions to the session's events. If a reviewer
   asks for changes, `vise sessions follow-up` spawns a new session on the same
   branch with the review threads already in its input.

## The pieces

| Component | What it is | Where it runs |
|-----------|------------|---------------|
| `vise-server` | HTTP API, FIFO scheduler, lease sweeper and PR tracker, backed by Postgres | A container on any machine; the installer puts it in Docker on your laptop |
| `vise-host` | Claims sessions and drives the agent harness in a per-session workspace | Natively on each machine you enroll, because it spawns the locally installed agent |
| `vise` | The CLI: sessions, host enrollment, and a `vise host` supervisor for the local host process | Wherever you work |

The API is documented with OpenAPI (Swagger UI is served at `/docs`), and the
Rust client the CLI and host use is generated from that spec, so any
integration you write speaks the same contract.

## Harnesses

A harness is the agent runtime a host launches for a session. v0.1.0 ships two:

- **`claude-code`** (the default) runs Claude Code over ACP. It needs the
  `claude` CLI installed on the host and uses whatever credentials that
  install already has.
- **`echo`** is a fake runtime that echoes the prompt back as agent messages.
  It needs no agent and no credentials, so it is the quickest way to prove the
  server, host and CLI are wired up.

## What vise is not

vise does not host models, proxy API keys, or run agents in the cloud on your
behalf. Hosts are machines you own or rent, and the agent on them uses the
credentials that are already there. Permission prompts from the agent are
auto-approved in v0.1.0 and recorded as events, so treat a host as a machine
that agent is allowed to act on.

## Next steps

- [Install](/getting-started/install/) the stack with one command.
- Run your first session in the [Quickstart](/getting-started/quickstart/).
- Read [Concepts](/getting-started/concepts/) for the full vocabulary:
  sessions, hosts, leases, outcomes and PR tracking.
