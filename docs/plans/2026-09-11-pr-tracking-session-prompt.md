# Session prompt: PR review tracking + follow-up sessions

Implement post-completion PR tracking and follow-up sessions. Today a session's story ends at `Completed` with `outcome.pr_url`; after this change, the server tracks the PR until it merges or closes, and a human can spawn a follow-up session to address review feedback.

## Domain model

Add to the sessions domain (`crates/vise-core/src/sessions/model.rs`):

```rust
pub struct PrStatus {
    pub state: PrState,              // review_pending | changes_requested | approved | merged | closed | sync_error
    pub checks: Option<ChecksState>, // pending | passing | failing
    pub last_synced_at: DateTime<Utc>,
}
```

- Stored on the session row (nullable JSONB column is fine). Populated only when `outcome.kind == "pr_opened"`.
- `merged` and `closed` are terminal: once reached, the session leaves the poller's work list forever.
- `PrState` is *derived*, not raw: reduce GitHub's review list to one enum — latest review per reviewer wins, any outstanding `changes_requested` beats approvals, GitHub's own merged/closed flags decide terminal states. Do NOT persist raw review comments.
- Add `parent_session_id: Option<String>` to `Session` (nullable column, FK to sessions).
- Add a new outcome kind `"pr_updated"` (same fields as `pr_opened`) for follow-up sessions that push to an existing PR.
- Migration style: merge schema changes into the existing `0001` migration rather than adding new numbered migrations.

## State transitions are session events

The existing `session_events` stream is the transition log:

- The poller appends an event ONLY when observed state differs from the snapshot — e.g. `{"type": "pr_state_changed", "from": "review_pending", "to": "changes_requested"}` and `{"type": "checks_state_changed", "from": "pending", "to": "failing"}`. Never one event per poll tick.
- Snapshot update + event append happen in ONE transaction, so they cannot drift.
- Snapshot answers "what is"; events answer "what happened". `sessions get` reads the snapshot; watching a completed session tails PR-state events.
- After `finished_at` no agent writes events, so the poller has the seq space to itself.

## The poller

A background tokio task in `vise-server`, spawned at startup next to the HTTP listener:

- Every tick (60s default, configurable), query the work list: sessions with `outcome.kind = "pr_opened"` and `pr_status` null or non-terminal. Use `FOR UPDATE SKIP LOCKED` so multiple server instances split work without leader election; transitions are compare-then-write so double-polling is correct anyway.
- Per session: fetch the PR (open/merged/closed, head branch, head SHA), the reviews list, and check runs for the head SHA. Reduce to `PrState` + `ChecksState`. On change: update snapshot + append event in one transaction. On no change: touch `last_synced_at` only.
- Credentials: reuse the stored GitHub credential from the generic credentials endpoint. Document that the token needs PR-read and checks-read scopes.
- One PR's fetch failure logs and skips; next tick retries. A persistent 403/404 sets `state = sync_error` rather than going silently stale. Rate-limit responses back off the whole tick. The loop never dies while the server runs.

## Follow-up sessions

`POST /sessions/{id}/follow-up` and CLI `vise sessions follow-up <id> [--instructions "..."]`:

- Validate: parent has a `pr_opened` outcome and its PR is still open per the snapshot. Merged/closed → error.
- Create a new session with `parent_session_id` set:
  - Agent config (harness, model, MCP servers) inherited from the parent unless overridden.
  - Environment: same repo, `base_branch` = the PR's *head* branch, so the agent pushes to the existing branch and the same PR updates.
  - Input: composed server-side at dispatch time. Fetch current review comment threads (with file/line context) and failing check names using the server's credential, and inline them with the user's optional instructions. The agent needs no extra scopes, and the review content is frozen in the session record.
- The follow-up's outcome is `pr_updated` with the same `pr_url`. PR tracking stays with the root session that opened the PR — one PR, one tracking stream. Follow-ups chain (`parent_session_id`), always resolving tracking to the root.

## Surface changes

- `sessions get` / `sessions list` render PR state and checks state when present.
- `--watch` on a completed session tails PR-state events (i.e. "watch this PR to merge").
- Regenerate the OpenAPI spec and the `vise-client` crate. Progenitor 0.15 constraints apply: endpoints must have a single success response shape, and no OpenAPI 3.1-style nulls — model `PrState`/`ChecksState` as plain string enums.

## Explicitly out of scope

- Webhooks (60s polling staleness is fine for a human-paced loop).
- Auto follow-up on `changes_requested` — human-triggered only.
- Notifications of any kind.
- Persisting raw review comments.

## Testing

- Unit: the review-reducer is pure — table-driven tests including: same reviewer requests changes then approves; stale reviews after force-push; zero check runs; mixed approvals with one outstanding changes_requested.
- Unit: transition detector (old snapshot + new observation → event or no-op).
- Integration: poller against a mocked GitHub API — snapshot+event in one transaction, no event on no-change, terminal states leave the work list, sync_error on persistent failure.
- Integration: follow-up endpoint — composes input containing review comments, inherits agent config, targets the head branch, rejects merged/closed PRs.
- Run `just check` (fmt, clippy, tests, OpenAPI drift, sqlx cache drift) before opening the PR. Update the sqlx offline cache if queries changed.

Open a PR when done.
