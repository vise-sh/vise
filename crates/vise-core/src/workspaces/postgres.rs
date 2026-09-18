use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{
    model::{Workspace, WorkspaceId},
    repository::WorkspaceRepository,
};

pub struct PostgresWorkspaceRepository {
    pool: PgPool,
}

impl PostgresWorkspaceRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct WorkspaceRow {
    id: String,
    name: String,
    settings: serde_json::Value,
    created_at: DateTime<Utc>,
}

impl From<WorkspaceRow> for Workspace {
    fn from(row: WorkspaceRow) -> Self {
        Workspace {
            id: WorkspaceId::new(row.id),
            name: row.name,
            settings: row.settings,
            created_at: row.created_at,
        }
    }
}

#[async_trait]
impl WorkspaceRepository for PostgresWorkspaceRepository {
    async fn create(&self, workspace: Workspace) -> anyhow::Result<Workspace> {
        sqlx::query!(
            r#"
            INSERT INTO workspaces (id, name, settings, created_at)
            VALUES ($1, $2, $3, $4)
            "#,
            workspace.id.as_str(),
            workspace.name,
            workspace.settings,
            workspace.created_at
        )
        .execute(&self.pool)
        .await?;

        Ok(workspace)
    }

    async fn get(&self, id: &WorkspaceId) -> anyhow::Result<Option<Workspace>> {
        let row = sqlx::query_as!(
            WorkspaceRow,
            r#"
            SELECT id, name, settings, created_at
            FROM workspaces
            WHERE id = $1
            "#,
            id.as_str()
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Workspace::from))
    }

    async fn list(&self) -> anyhow::Result<Vec<Workspace>> {
        let rows = sqlx::query_as!(
            WorkspaceRow,
            r#"
            SELECT id, name, settings, created_at
            FROM workspaces
            ORDER BY created_at, id
            "#
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Workspace::from).collect())
    }

    async fn delete(&self, id: &WorkspaceId) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM workspaces
            WHERE id = $1
            "#,
            id.as_str()
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
