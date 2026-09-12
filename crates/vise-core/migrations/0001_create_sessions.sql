CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    agent JSONB NOT NULL,
    environment JSONB NOT NULL,
    input TEXT NOT NULL,
    status TEXT NOT NULL,
    host_id TEXT,
    lease_expires_at TIMESTAMPTZ,
    started_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    stop_reason TEXT,
    error TEXT,
    outcome JSONB,
    -- Derived PR tracking snapshot (PrStatus). Populated by the server-side
    -- poller only for sessions whose outcome is "pr_opened".
    pr_status JSONB,
    -- Set on follow-up sessions; chains back to the session that opened the PR.
    parent_session_id TEXT REFERENCES sessions(id),
    cancel_requested BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_created_at_idx
    ON sessions (created_at DESC);

CREATE INDEX sessions_pending_created_at_idx
    ON sessions (created_at)
    WHERE status = 'pending';

-- The PR poller's work list: PRs opened by a session that have not yet
-- reached a terminal (merged/closed) state.
CREATE INDEX sessions_pr_tracking_idx
    ON sessions ((pr_status ->> 'last_synced_at'))
    WHERE outcome ->> 'kind' = 'pr_opened'
      AND (pr_status IS NULL OR pr_status ->> 'state' NOT IN ('merged', 'closed'));

CREATE INDEX sessions_parent_session_id_idx
    ON sessions (parent_session_id)
    WHERE parent_session_id IS NOT NULL;

CREATE TABLE hosts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    token_hash TEXT NOT NULL UNIQUE,
    last_seen_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE session_events (
    session_id text NOT NULL REFERENCES sessions(id),
    seq        bigint NOT NULL,          -- per-session, monotonically increasing
    payload    jsonb NOT NULL,           -- raw ACP SessionNotification, or a
                                         -- server-side PR tracking transition
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, seq)
);
