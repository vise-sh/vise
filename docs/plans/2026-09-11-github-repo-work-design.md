# GitHub Repo Work — Design

Enable a vise session to do real work on a GitHub repository: the agent works
in a real clone with credentials and is instructed to branch, commit, push,
and open a PR itself.

## Decisions

- **End state:** branch + PR on GitHub.
- **Auth:** GitHub App. Server holds the private key; hosts get short-lived
  installation tokens through a **generic credentials endpoint** (not a
  GitHub-specific route) so future providers (Linear, etc.) reuse the same
  protocol.
- **Git driver:** the agent drives git (branch, commit, push, `gh pr create`).
  vise-host provisions the workspace and credentials, then observes the
  outcome. A host-side safety net (auto-commit/push/PR) is a possible later
  addition if agent reliability disappoints.

## Environment config

Flesh out the stubbed `environment` on `Session` with a new kind:

```jsonc
{
  "kind": "github_repo",
  "repo": "owner/name",        // which repo to clone
  "base_branch": "main"        // optional; defaults to repo default branch
}
```

Validated at create time by `vise-api`. CLI grows flags such as
`vise session create --repo owner/name`.

## Credentials endpoint & providers (server)

Hosts fetch per-session secrets through one generic endpoint, so the wire
protocol never changes as new secret types are added:

- `POST /hosts/sessions/{id}/credentials` with body `{ "provider": "github" }`.
  Host-authenticated and only valid for the host holding the session lease.
- Response: `{ "provider": "github", "secret": "...", "expires_at": "..." }`
  (`expires_at` nullable — some future providers issue non-expiring keys).
- Errors: 422 when the provider isn't applicable to the session (e.g.
  `github` on a session with no repo), 503 when the server has no config
  for that provider.
- Server internals: a `CredentialProvider` trait (`name()`,
  `issue(&session)`); `AppState` holds a provider registry. Adding Linear
  later is one new impl + config, zero API changes.

The v1 GitHub provider:

- Register a "vise" GitHub App with permissions: contents read/write,
  pull requests read/write. Install it on the repos/orgs vise may touch.
  (The user already has a GitHub App — configure, don't re-register.)
- `vise-server` holds App ID + private key (env/config). Only the server
  ever sees the private key.
- `issue()` resolves the installation for the session's repo and mints an
  installation access token scoped to that repo (~1h TTL). Hosts may
  re-issue repeatedly to refresh during long sessions.
- Implementation: minimal JWT + reqwest in the server only. Hosts need no
  GitHub API dependency.

## Host workspace & credential injection

When `vise-host` claims a `github_repo` session, before spawning the runtime:

1. `POST /hosts/sessions/{id}/credentials` (`provider: "github"`) for a
   fresh token.
2. Clone into `{workdir}/sessions/{session_id}/repo` — shallow (`--depth 50`,
   single base branch), checkout `base_branch`.
3. Set clone-local git identity: `user.name = "vise[bot]"`, `user.email` =
   the App's noreply address (never touches host-global config).
4. Spawn the ACP runtime with **cwd = the clone**.

Credentials into the agent subprocess:

- `GITHUB_TOKEN` env var — `gh` CLI picks this up automatically.
- Git push auth via a clone-local `credential.helper` script that reads a
  token file vise-host keeps fresh. Mid-session refresh works by rewriting
  the token file — no agent restart needed.

Prompt scaffolding: vise-host prepends a preamble to the session input —
"You are working in a clone of `owner/name` based on `main`. When done:
create a branch, commit with clear messages, push, open a PR with
`gh pr create`, and report the PR URL."

Cleanup: workspace removed when the session reaches a terminal state
(`keep_workspaces = true` config to retain for debugging).

## Outcome tracking

The agent drives git, so vise observes rather than controls. After the agent
finishes, vise-host inspects the workspace and records an `outcome`
(new nullable JSONB column on sessions, shown by `vise-cli watch`):

- `pr_opened { url }` — via `gh pr view --json url` on the agent's branch,
  falling back to parsing the agent's final message
- `pushed_no_pr { branch }` — branch pushed but no PR
- `uncommitted_changes` — agent edited files but never committed; forces
  workspace retention for that session so work isn't lost
- `no_changes` — agent finished without touching anything

## Failure modes

- **Clone fails / App not installed on repo** → session fails fast with a
  clear error event before the agent starts.
- **Token expiry mid-session** → host refreshes on a timer (~45 min) and
  rewrites the token file the credential helper reads.
- **Host crash** → existing lease-expiry sweeper handles the session;
  orphaned workspaces cleaned on next host startup (dirs whose session is
  terminal).

## Testing

- Unit: environment config validation; token endpoint with mocked GitHub.
- Integration: host flow against a local bare repo over `file://` with a
  fake token service — CI needs no real GitHub.
- One manual end-to-end against a real scratch repo.
