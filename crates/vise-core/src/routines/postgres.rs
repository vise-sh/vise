use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{
    model::{Routine, SessionSpec},
    repository::RoutineRepository,
};
use crate::workspaces::model::WorkspaceId;

pub struct PostgresRoutineRepository {
    pool: PgPool,
}

impl PostgresRoutineRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct RoutineRow {
    id: String,
    workspace_id: String,
    name: String,
    cron: String,
    timezone: String,
    spec: sqlx::types::Json<SessionSpec>,
    enabled: bool,
    next_run_at: DateTime<Utc>,
    last_fired_at: Option<DateTime<Utc>>,
    last_session_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<RoutineRow> for Routine {
    fn from(row: RoutineRow) -> Self {
        Routine {
            id: row.id,
            workspace_id: WorkspaceId::new(row.workspace_id),
            name: row.name,
            cron: row.cron,
            timezone: row.timezone,
            spec: row.spec.0,
            enabled: row.enabled,
            next_run_at: row.next_run_at,
            last_fired_at: row.last_fired_at,
            last_session_id: row.last_session_id,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[async_trait]
impl RoutineRepository for PostgresRoutineRepository {
    async fn create(&self, routine: Routine) -> anyhow::Result<Routine> {
        sqlx::query(
            r#"
            INSERT INTO routines (
                id,
                workspace_id,
                name,
                cron,
                timezone,
                spec,
                enabled,
                next_run_at,
                last_fired_at,
                last_session_id,
                created_at,
                updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(&routine.id)
        .bind(routine.workspace_id.as_str())
        .bind(&routine.name)
        .bind(&routine.cron)
        .bind(&routine.timezone)
        .bind(sqlx::types::Json(&routine.spec))
        .bind(routine.enabled)
        .bind(routine.next_run_at)
        .bind(routine.last_fired_at)
        .bind(&routine.last_session_id)
        .bind(routine.created_at)
        .bind(routine.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(routine)
    }

    async fn get(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<Option<Routine>> {
        let row = sqlx::query_as!(
            RoutineRow,
            r#"
            SELECT
                id,
                workspace_id,
                name,
                cron,
                timezone,
                spec as "spec: _",
                enabled,
                next_run_at,
                last_fired_at,
                last_session_id,
                created_at,
                updated_at
            FROM routines
            WHERE workspace_id = $1 AND id = $2
            "#,
            workspace.as_str(),
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Routine::from))
    }

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Routine>> {
        let rows = sqlx::query_as!(
            RoutineRow,
            r#"
            SELECT
                id,
                workspace_id,
                name,
                cron,
                timezone,
                spec as "spec: _",
                enabled,
                next_run_at,
                last_fired_at,
                last_session_id,
                created_at,
                updated_at
            FROM routines
            WHERE workspace_id = $1
            ORDER BY created_at DESC
            "#,
            workspace.as_str()
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Routine::from).collect())
    }

    async fn update(&self, routine: Routine) -> anyhow::Result<Option<Routine>> {
        let row = sqlx::query_as!(
            RoutineRow,
            r#"
            UPDATE routines
            SET name = $3,
                cron = $4,
                timezone = $5,
                spec = $6,
                enabled = $7,
                next_run_at = $8,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2
            RETURNING
                id,
                workspace_id,
                name,
                cron,
                timezone,
                spec as "spec: _",
                enabled,
                next_run_at,
                last_fired_at,
                last_session_id,
                created_at,
                updated_at
            "#,
            routine.workspace_id.as_str(),
            routine.id,
            routine.name,
            routine.cron,
            routine.timezone,
            sqlx::types::Json(&routine.spec) as _,
            routine.enabled,
            routine.next_run_at
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Routine::from))
    }

    async fn delete(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM routines
            WHERE workspace_id = $1 AND id = $2
            "#,
            workspace.as_str(),
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn claim_due(&self, now: DateTime<Utc>, limit: i64) -> anyhow::Result<Vec<Routine>> {
        let rows = sqlx::query_as!(
            RoutineRow,
            r#"
            UPDATE routines
            SET next_run_at = $1 + interval '5 minutes'
            WHERE id IN (
                SELECT id FROM routines
                WHERE enabled AND next_run_at <= $1
                ORDER BY next_run_at
                LIMIT $2
                FOR UPDATE SKIP LOCKED
            )
            RETURNING
                id,
                workspace_id,
                name,
                cron,
                timezone,
                spec as "spec: _",
                enabled,
                next_run_at,
                last_fired_at,
                last_session_id,
                created_at,
                updated_at
            "#,
            now,
            limit
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Routine::from).collect())
    }

    async fn record_fire(
        &self,
        id: &str,
        next_run_at: DateTime<Utc>,
        last_fired_at: Option<DateTime<Utc>>,
        last_session_id: Option<String>,
    ) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            UPDATE routines
            SET next_run_at = $2,
                last_fired_at = $3,
                last_session_id = $4,
                updated_at = now()
            WHERE id = $1
            "#,
            id,
            next_run_at,
            last_fired_at,
            last_session_id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}
