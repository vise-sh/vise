use async_trait::async_trait;
use sqlx::PgPool;

use super::{model::Host, repository::HostRepository};

pub struct PostgresHostRepository {
    pool: PgPool,
}

impl PostgresHostRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl HostRepository for PostgresHostRepository {
    async fn create(&self, host: Host, token_hash: &str) -> anyhow::Result<Host> {
        sqlx::query!(
            r#"
            INSERT INTO hosts (id, name, token_hash, created_at)
            VALUES ($1, $2, $3, $4)
            "#,
            host.id,
            host.name,
            token_hash,
            host.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(host)
    }

    async fn list(&self) -> anyhow::Result<Vec<Host>> {
        let rows = sqlx::query_as!(
            Host,
            r#"
            SELECT id, name, last_seen_at, created_at
            FROM hosts
            ORDER BY created_at
            "#
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    async fn authenticate(&self, token_hash: &str) -> anyhow::Result<Option<Host>> {
        let row = sqlx::query_as!(
            Host,
            r#"
            UPDATE hosts
            SET last_seen_at = now()
            WHERE token_hash = $1
            RETURNING id, name, last_seen_at, created_at
            "#,
            token_hash
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row)
    }
}
