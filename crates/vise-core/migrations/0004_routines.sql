-- Routines: API-managed scheduled session spawns (VISE-237). A routine holds
-- a cron schedule and an inline session spec; a background scheduler spawns a
-- session per due routine, in the routine's own workspace. The spec JSONB
-- mirrors the public CreateSessionRequest shape (agent, environment, input).
CREATE TABLE routines (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
    name TEXT NOT NULL,
    cron TEXT NOT NULL,
    timezone TEXT NOT NULL,
    spec JSONB NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT true,
    next_run_at TIMESTAMPTZ NOT NULL,
    last_fired_at TIMESTAMPTZ,
    last_session_id TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- The scheduler's claim query: enabled routines that are due, oldest first.
CREATE INDEX routines_due_idx ON routines (next_run_at) WHERE enabled;

-- Routines are listed per workspace, newest first.
CREATE INDEX routines_workspace_idx ON routines (workspace_id, created_at DESC);

-- Link a spawned session back to its routine. NULL for hand-created sessions.
ALTER TABLE sessions ADD COLUMN routine_id TEXT;
CREATE INDEX sessions_routine_idx ON sessions (routine_id, created_at DESC)
    WHERE routine_id IS NOT NULL;
