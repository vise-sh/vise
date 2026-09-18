use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{model::Host, repository::HostRepository};
use crate::workspaces::model::WorkspaceId;

pub struct PostgresHostRepository {
    pool: PgPool,
}

impl PostgresHostRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct HostRow {
    id: String,
    workspace_id: String,
    name: String,
    ephemeral: bool,
    last_seen_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<HostRow> for Host {
    fn from(row: HostRow) -> Self {
        Host {
            id: row.id,
            workspace_id: WorkspaceId::new(row.workspace_id),
            name: row.name,
            ephemeral: row.ephemeral,
            last_seen_at: row.last_seen_at,
            created_at: row.created_at,
        }
    }
}

#[async_trait]
impl HostRepository for PostgresHostRepository {
    async fn create(&self, host: Host, token_hash: &str) -> anyhow::Result<Host> {
        sqlx::query!(
            r#"
            INSERT INTO hosts (id, workspace_id, name, ephemeral, token_hash, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            host.id,
            host.workspace_id.as_str(),
            host.name,
            host.ephemeral,
            token_hash,
            host.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(host)
    }

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Host>> {
        let rows = sqlx::query_as!(
            HostRow,
            r#"
            SELECT id, workspace_id, name, ephemeral, last_seen_at, created_at
            FROM hosts
            WHERE workspace_id = $1
            ORDER BY created_at
            "#,
            workspace.as_str()
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Host::from).collect())
    }

    async fn authenticate(&self, token_hash: &str) -> anyhow::Result<Option<Host>> {
        let row = sqlx::query_as!(
            HostRow,
            r#"
            UPDATE hosts
            SET last_seen_at = now()
            WHERE token_hash = $1
            RETURNING id, workspace_id, name, ephemeral, last_seen_at, created_at
            "#,
            token_hash
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Host::from))
    }

    async fn delete_ephemeral_unseen_since(&self, cutoff: DateTime<Utc>) -> anyhow::Result<u64> {
        // `sessions.host_id` carries no foreign key, so deleting a host
        // leaves finished sessions' `host_id` as a historical string — the
        // schema's existing stance. A host still holding a *running* session
        // is spared so the lease sweeper (not the reaper) decides that
        // session's fate; once the lease lapses the next pass deletes the
        // host.
        let result = sqlx::query!(
            r#"
            DELETE FROM hosts
            WHERE ephemeral
              AND COALESCE(last_seen_at, created_at) < $1
              AND NOT EXISTS (
                  SELECT 1 FROM sessions
                  WHERE sessions.host_id = hosts.id
                    AND sessions.status = 'running'
              )
            "#,
            cutoff
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }
}
