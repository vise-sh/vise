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
    cancel_requested BOOLEAN NOT NULL DEFAULT false,
    -- follow-up sessions point at the session whose PR they address
    parent_session_id TEXT REFERENCES sessions(id),
    -- derived PR snapshot ({state, checks, last_synced_at}); only for pr_opened outcomes
    pr_status JSONB,
    -- consecutive 401/403/404 polls; reaching the threshold sets pr_status.state = sync_error
    pr_sync_failures INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_created_at_idx
    ON sessions (created_at DESC);

CREATE INDEX sessions_pending_created_at_idx
    ON sessions (created_at)
    WHERE status = 'pending';

-- PR tracking work list: completed sessions that opened a PR. The poller
-- further filters out terminal (merged/closed) snapshots.
CREATE INDEX sessions_pr_tracking_idx
    ON sessions (created_at)
    WHERE outcome->>'kind' = 'pr_opened';

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
    payload    jsonb NOT NULL,           -- raw ACP SessionNotification
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, seq)
);
