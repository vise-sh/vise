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
    created_at TIMESTAMPTZ NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX sessions_created_at_idx
    ON sessions (created_at DESC);

CREATE INDEX sessions_pending_created_at_idx
    ON sessions (created_at)
    WHERE status = 'pending';

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
