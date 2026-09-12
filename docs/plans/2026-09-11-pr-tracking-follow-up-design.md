# PR Review Tracking + Follow-up Sessions — Design

A session's story used to end at `Completed` with `outcome.pr_url`. Now the
server tracks that PR until it merges or closes, and a human can spawn a
follow-up session that addresses review feedback on the same PR.

## Decisions

| Question | Decision |
|---|---|
| Where does PR state live? | `sessions.pr_status` JSONB snapshot (`{state, checks, last_synced_at}`), only on `pr_opened` outcomes |
| What is persisted? | The *derived* state only. Raw reviews/comments are read, reduced, and discarded |
| How are transitions recorded? | As `session_events` rows appended by the poller, only when observed state differs from the snapshot |
| Consistency | Snapshot update + event append commit in one transaction |
| Discovery | Polling (60s default). Webhooks are out of scope |
| Multi-instance | `FOR UPDATE SKIP LOCKED` per session; transitions are compare-then-write so double polling is harmless |
| Who tracks a PR? | The root session that opened it. Follow-ups report `pr_updated` and are never polled |
| Follow-up trigger | Human only (`POST /sessions/{id}/follow-up`, `vise sessions follow-up`) |

## Reducer (`vise_core::sessions::pr_tracking`)

Pure functions, table-tested:

- `reduce_reviews(reviews, head_sha)`:
  1. per reviewer, latest decisive review (approve / request changes /
     dismiss) wins; `COMMENTED`/`PENDING` never change standing;
  2. any outstanding `changes_requested` → `changes_requested`;
  3. an approval counts only if submitted against the current head
     (force-pushed-away approvals are stale) → `approved`;
  4. otherwise `review_pending`.
- `reduce_checks(check_runs)`: any failing conclusion → `failing`; any
  non-completed → `pending`; all done → `passing`; zero runs → `None`.
- GitHub's `merged` / `state == closed` flags decide `merged` / `closed`.
- `detect_transitions(previous, state, checks)` yields zero, one, or two
  events; the first observation emits `from: null`.

## Poller (`vise_api::pr_tracking::PrTracker`)

Spawned by `vise-server` when the GitHub App credential is configured.
Each tick:

1. `claim_pr_tracking(tick_start, handled)` opens a transaction and locks the
   next session with `outcome.kind = 'pr_opened'`, a non-terminal (or
   absent) snapshot, and `last_synced_at < tick_start`, via
   `FOR UPDATE SKIP LOCKED`.
2. Mint an installation token through the same `CredentialProvider` hosts
   use (cached per repo per tick); fetch PR, reviews, check runs at head.
3. Reduce; diff against the snapshot; `record_pr_status` writes the new
   snapshot and appends events (seq continues after the last event — after
   `finished_at` the poller is the only writer) and commits.
4. On no change only `last_synced_at` moves.

Failure handling:

- Transient errors (5xx, network, token minting): rollback, skip, retry next tick.
- 401/403/404: bump `pr_sync_failures`; at 3 consecutive, write
  `state = sync_error` (with its transition event). Not terminal: a later
  readable poll transitions back.
- Rate limit (403/429 + `x-ratelimit-remaining: 0`, or `retry-after`): stop
  the tick, sleep `max(interval, retry_after)`.
- Unparseable `pr_url`: `sync_error` immediately.
- The loop never returns; every error is logged.

## Follow-up (`POST /sessions/{id}/follow-up`)

1. Resolve the root by walking `parent_session_id`.
2. Root must have a `pr_opened` outcome (422) with a non-terminal snapshot
   (409); GitHub's live answer is checked too, since the snapshot lags.
3. Fetch review summaries, inline comment threads (grouped by
   `in_reply_to_id`, with file/line and diff hunk), and failing check names
   with the server credential; compose the input server-side so the agent
   needs no extra scopes and the feedback is frozen in the session record.
4. Create a session: agent inherited from the parent unless overridden,
   environment `github_repo` on the same repo with `base_branch` = PR head
   branch, `parent_session_id` = the requested session.

`vise-host` sees `parent_session_id` and swaps the preamble ("push to the
same branch, do not open a new PR") and reports `pr_updated` instead of
`pr_opened`.

## Surface

- `sessions get` / `ls` include `pr_status`.
- `sessions watch` on a completed `pr_opened` session tails
  `pr_state_changed` / `checks_state_changed` events until merged/closed
  (the SSE stream's terminal condition extends to the PR when tracking is
  enabled on the server).
- OpenAPI: `PrState`/`ChecksState` are plain string enums; the follow-up
  endpoint has a single 201 body (`Session`).

## Out of scope

Webhooks, auto follow-up on `changes_requested`, notifications, persisting
raw review comments.
