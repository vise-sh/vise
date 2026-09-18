use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{model::EnrollmentToken, repository::EnrollmentTokenRepository};
use crate::workspaces::model::WorkspaceId;

pub struct PostgresEnrollmentTokenRepository {
    pool: PgPool,
}

impl PostgresEnrollmentTokenRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct EnrollmentTokenRow {
    id: String,
    workspace_id: String,
    max_uses: Option<i64>,
    uses: i64,
    revoked_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
}

impl From<EnrollmentTokenRow> for EnrollmentToken {
    fn from(row: EnrollmentTokenRow) -> Self {
        EnrollmentToken {
            id: row.id,
            workspace_id: WorkspaceId::new(row.workspace_id),
            max_uses: row.max_uses,
            uses: row.uses,
            revoked_at: row.revoked_at,
            created_at: row.created_at,
        }
    }
}

#[async_trait]
impl EnrollmentTokenRepository for PostgresEnrollmentTokenRepository {
    async fn create(
        &self,
        token: EnrollmentToken,
        token_hash: &str,
    ) -> anyhow::Result<EnrollmentToken> {
        sqlx::query!(
            r#"
            INSERT INTO enrollment_tokens (id, workspace_id, token_hash, max_uses, uses, created_at)
            VALUES ($1, $2, $3, $4, $5, $6)
            "#,
            token.id,
            token.workspace_id.as_str(),
            token_hash,
            token.max_uses,
            token.uses,
            token.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(token)
    }

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<EnrollmentToken>> {
        let rows = sqlx::query_as!(
            EnrollmentTokenRow,
            r#"
            SELECT id, workspace_id, max_uses, uses, revoked_at, created_at
            FROM enrollment_tokens
            WHERE workspace_id = $1
            ORDER BY created_at DESC
            "#,
            workspace.as_str()
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(EnrollmentToken::from).collect())
    }

    async fn revoke(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<EnrollmentToken>> {
        let row = sqlx::query_as!(
            EnrollmentTokenRow,
            r#"
            UPDATE enrollment_tokens
            SET revoked_at = COALESCE(revoked_at, now())
            WHERE workspace_id = $1 AND id = $2
            RETURNING id, workspace_id, max_uses, uses, revoked_at, created_at
            "#,
            workspace.as_str(),
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(EnrollmentToken::from))
    }

    async fn consume(&self, token_hash: &str) -> anyhow::Result<Option<EnrollmentToken>> {
        // One statement: the row lock the UPDATE takes serializes concurrent
        // exchanges, and each re-checks the WHERE clause after the previous
        // increment commits, so `uses` can never pass `max_uses`.
        let row = sqlx::query_as!(
            EnrollmentTokenRow,
            r#"
            UPDATE enrollment_tokens
            SET uses = uses + 1
            WHERE token_hash = $1
              AND revoked_at IS NULL
              AND (max_uses IS NULL OR uses < max_uses)
            RETURNING id, workspace_id, max_uses, uses, revoked_at, created_at
            "#,
            token_hash
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(EnrollmentToken::from))
    }
}
