-- Reusable enrollment tokens: a workspace-scoped secret (`venroll_` prefix,
-- shown exactly once at mint) that a booting host exchanges for its own
-- `vhost_` token. Only the SHA-256 hash is stored, like host tokens.
CREATE TABLE enrollment_tokens (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id),
    token_hash TEXT NOT NULL UNIQUE,
    -- NULL = unlimited exchanges.
    max_uses BIGINT,
    uses BIGINT NOT NULL DEFAULT 0,
    revoked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX enrollment_tokens_workspace_created_at_idx
    ON enrollment_tokens (workspace_id, created_at DESC);

-- Hosts created through an enrollment token are ephemeral: the reaper in
-- vise-server deletes them once they stop heartbeating. Hosts enrolled by
-- hand stay forever.
ALTER TABLE hosts
    ADD COLUMN ephemeral BOOLEAN NOT NULL DEFAULT false;
