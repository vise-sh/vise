-- Workspace tenancy. Every host and session belongs to exactly one
-- workspace. The out-of-the-box server is single-tenant and puts everything
-- in the seeded `default` workspace; the column exists so the same schema
-- can hold many tenants without a further migration.
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    settings JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO workspaces (id, name) VALUES ('default', 'Default');

-- Backfill existing rows to `default` through a column default, then drop
-- the default so every insert has to name its workspace explicitly.
ALTER TABLE hosts
    ADD COLUMN workspace_id TEXT NOT NULL DEFAULT 'default' REFERENCES workspaces(id);
ALTER TABLE hosts
    ALTER COLUMN workspace_id DROP DEFAULT;

-- Host names are unique per workspace, not globally.
ALTER TABLE hosts DROP CONSTRAINT hosts_name_key;
ALTER TABLE hosts ADD CONSTRAINT hosts_workspace_id_name_key UNIQUE (workspace_id, name);

ALTER TABLE sessions
    ADD COLUMN workspace_id TEXT NOT NULL DEFAULT 'default' REFERENCES workspaces(id);
ALTER TABLE sessions
    ALTER COLUMN workspace_id DROP DEFAULT;

-- Listing and claiming are always scoped to one workspace.
DROP INDEX sessions_created_at_idx;
CREATE INDEX sessions_workspace_created_at_idx
    ON sessions (workspace_id, created_at DESC);

DROP INDEX sessions_pending_created_at_idx;
CREATE INDEX sessions_pending_created_at_idx
    ON sessions (workspace_id, created_at)
    WHERE status = 'pending';

-- session_events carries no workspace column: scope flows through the
-- session foreign key.
