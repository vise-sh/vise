use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{
    model::{
        Agent, Environment, NewSessionEvent, PrStatus, Session, SessionOutcome, SessionStatus,
    },
    repository::{PrSync, SessionRepository},
};
use crate::workspaces::model::WorkspaceId;

pub struct PostgresSessionRepository {
    pool: PgPool,
}

impl PostgresSessionRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

struct SessionRow {
    id: String,
    workspace_id: String,
    agent: sqlx::types::Json<Agent>,
    environment: sqlx::types::Json<Environment>,
    input: String,
    status: String,
    host_id: Option<String>,
    lease_expires_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    stop_reason: Option<String>,
    error: Option<String>,
    outcome: Option<sqlx::types::Json<SessionOutcome>>,
    pr_status: Option<sqlx::types::Json<PrStatus>>,
    parent_session_id: Option<String>,
    routine_id: Option<String>,
    cancel_requested: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<SessionRow> for Session {
    fn from(row: SessionRow) -> Self {
        Session {
            id: row.id,
            workspace_id: WorkspaceId::new(row.workspace_id),
            agent: row.agent.0,
            environment: row.environment.0,
            input: row.input,
            status: parse_status(&row.status),
            host_id: row.host_id,
            lease_expires_at: row.lease_expires_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
            stop_reason: row.stop_reason,
            error: row.error,
            outcome: row.outcome.map(|j| j.0),
            pr_status: row.pr_status.map(|j| j.0),
            parent_session_id: row.parent_session_id,
            routine_id: row.routine_id,
            cancel_requested: row.cancel_requested,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

#[async_trait]
impl SessionRepository for PostgresSessionRepository {
    async fn create(&self, session: Session) -> anyhow::Result<Session> {
        sqlx::query(
            r#"
            INSERT INTO sessions (
                id,
                workspace_id,
                agent,
                environment,
                input,
                status,
                parent_session_id,
                routine_id,
                created_at,
                updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            "#,
        )
        .bind(&session.id)
        .bind(session.workspace_id.as_str())
        .bind(sqlx::types::Json(&session.agent))
        .bind(sqlx::types::Json(&session.environment))
        .bind(&session.input)
        .bind(status_str(&session.status))
        .bind(&session.parent_session_id)
        .bind(&session.routine_id)
        .bind(session.created_at)
        .bind(session.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(session)
    }

    async fn get(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            FROM sessions
            WHERE workspace_id = $1 AND id = $2
            "#,
            workspace.as_str(),
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Session>> {
        let rows = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            FROM sessions
            WHERE workspace_id = $1
            ORDER BY created_at DESC
            "#,
            workspace.as_str()
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    async fn list_by_routine(
        &self,
        workspace: &WorkspaceId,
        routine_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        let rows = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            FROM sessions
            WHERE workspace_id = $1 AND routine_id = $2
            ORDER BY created_at DESC
            "#,
            workspace.as_str(),
            routine_id
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    async fn delete(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM sessions
            WHERE workspace_id = $1 AND id = $2
            "#,
            workspace.as_str(),
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_events(
        &self,
        workspace: &WorkspaceId,
        id: &str,
        after_seq: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<super::model::SessionEvent>> {
        // session_events has no workspace column; the join to sessions is
        // what keeps another workspace's session id from reading these.
        let rows = sqlx::query!(
            r#"
            SELECT e.session_id, e.seq, e.payload, e.created_at
            FROM session_events e
            JOIN sessions s ON s.id = e.session_id
            WHERE s.workspace_id = $1 AND e.session_id = $2 AND e.seq > $3
            ORDER BY e.seq
            LIMIT $4
            "#,
            workspace.as_str(),
            id,
            after_seq,
            limit
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| super::model::SessionEvent {
                session_id: row.session_id,
                seq: row.seq,
                payload: row.payload,
                created_at: row.created_at,
            })
            .collect())
    }

    async fn claim_pending(&self, host_id: &str) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            UPDATE sessions
            SET status = 'running',
                host_id = $1,
                lease_expires_at = now() + interval '60 seconds',
                started_at = now(),
                updated_at = now()
            WHERE id = (
                SELECT id FROM sessions
                WHERE status = 'pending'
                  AND workspace_id = (SELECT workspace_id FROM hosts WHERE id = $1)
                ORDER BY created_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            "#,
            host_id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn heartbeat(&self, host_id: &str, id: &str) -> anyhow::Result<Option<bool>> {
        let row = sqlx::query!(
            r#"
            UPDATE sessions
            SET lease_expires_at = now() + interval '60 seconds',
                updated_at = now()
            WHERE id = $2 AND host_id = $1 AND status = 'running'
              AND workspace_id = (SELECT workspace_id FROM hosts WHERE id = $1)
            RETURNING cancel_requested
            "#,
            host_id,
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|row| row.cancel_requested))
    }

    async fn append_events(
        &self,
        host_id: &str,
        id: &str,
        events: &[NewSessionEvent],
    ) -> anyhow::Result<Option<()>> {
        let held = sqlx::query_scalar!(
            r#"
            SELECT 1 FROM sessions
            WHERE id = $2 AND host_id = $1 AND status = 'running'
              AND workspace_id = (SELECT workspace_id FROM hosts WHERE id = $1)
            "#,
            host_id,
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        if held.is_none() {
            return Ok(None);
        }

        let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
        let payloads: Vec<serde_json::Value> = events.iter().map(|e| e.payload.clone()).collect();

        sqlx::query!(
            r#"
            INSERT INTO session_events (session_id, seq, payload)
            SELECT $1, seq, payload
            FROM UNNEST($2::bigint[], $3::jsonb[]) AS t(seq, payload)
            ON CONFLICT DO NOTHING
            "#,
            id,
            &seqs,
            &payloads
        )
        .execute(&self.pool)
        .await?;

        Ok(Some(()))
    }

    async fn finish(
        &self,
        host_id: &str,
        id: &str,
        status: SessionStatus,
        stop_reason: Option<String>,
        error: Option<String>,
        outcome: Option<SessionOutcome>,
    ) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            UPDATE sessions
            SET status = $3,
                stop_reason = $4,
                error = $5,
                outcome = $6,
                lease_expires_at = NULL,
                finished_at = now(),
                updated_at = now()
            WHERE id = $2 AND host_id = $1 AND status = 'running'
              AND workspace_id = (SELECT workspace_id FROM hosts WHERE id = $1)
            RETURNING
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            "#,
            host_id,
            id,
            status_str(&status),
            stop_reason.as_deref(),
            error.as_deref(),
            outcome.map(sqlx::types::Json) as _
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn request_cancel(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            UPDATE sessions
            SET cancel_requested = true,
                status = CASE WHEN status = 'pending' THEN 'cancelled' ELSE status END,
                finished_at = CASE WHEN status = 'pending' THEN now() ELSE finished_at END,
                updated_at = now()
            WHERE workspace_id = $1 AND id = $2 AND status IN ('pending', 'running')
            RETURNING
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            "#,
            workspace.as_str(),
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn expire_leases(&self) -> anyhow::Result<u64> {
        let result = sqlx::query!(
            r#"
            UPDATE sessions
            SET status = 'failed',
                error = 'lease expired',
                lease_expires_at = NULL,
                finished_at = now(),
                updated_at = now()
            WHERE status = 'running' AND lease_expires_at < now()
            "#
        )
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    async fn pr_tracking_work_list(&self, limit: i64) -> anyhow::Result<Vec<Session>> {
        let rows = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            FROM sessions
            WHERE outcome ->> 'kind' = 'pr_opened'
              AND (pr_status IS NULL OR pr_status ->> 'state' NOT IN ('merged', 'closed'))
            ORDER BY pr_status ->> 'last_synced_at' NULLS FIRST, created_at
            LIMIT $1
            "#,
            limit
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    async fn begin_pr_sync(&self, id: &str) -> anyhow::Result<Option<Box<dyn PrSync>>> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
                workspace_id,
                agent as "agent: _",
                environment as "environment: _",
                input,
                status,
                host_id,
                lease_expires_at,
                started_at,
                finished_at,
                stop_reason,
                error,
                outcome as "outcome: _",
                pr_status as "pr_status: _",
                parent_session_id,
                routine_id,
                cancel_requested,
                created_at,
                updated_at
            FROM sessions
            WHERE id = $1
              AND outcome ->> 'kind' = 'pr_opened'
              AND (pr_status IS NULL OR pr_status ->> 'state' NOT IN ('merged', 'closed'))
            FOR UPDATE SKIP LOCKED
            "#,
            id
        )
        .fetch_optional(&mut *tx)
        .await?;

        Ok(row.map(|row| {
            Box::new(PostgresPrSync {
                tx,
                session: Session::from(row),
            }) as Box<dyn PrSync>
        }))
    }
}

/// Holds the `FOR UPDATE` lock on a tracked session between observing the
/// PR and writing the result. The lock also serialises event `seq`
/// allocation: after `finished_at` no host writes events, so the poller owns
/// the sequence space and `MAX(seq) + n` is safe under the row lock.
struct PostgresPrSync {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    session: Session,
}

#[async_trait]
impl PrSync for PostgresPrSync {
    fn session(&self) -> &Session {
        &self.session
    }

    async fn commit(
        mut self: Box<Self>,
        status: PrStatus,
        events: Vec<serde_json::Value>,
    ) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            UPDATE sessions
            SET pr_status = $2,
                updated_at = now()
            WHERE id = $1
            "#,
            self.session.id,
            sqlx::types::Json(&status) as _
        )
        .execute(&mut *self.tx)
        .await?;

        if !events.is_empty() {
            sqlx::query!(
                r#"
                INSERT INTO session_events (session_id, seq, payload)
                SELECT
                    $1,
                    COALESCE((SELECT MAX(seq) FROM session_events WHERE session_id = $1), 0)
                        + t.ordinality,
                    t.payload
                FROM UNNEST($2::jsonb[]) WITH ORDINALITY AS t(payload, ordinality)
                "#,
                self.session.id,
                &events
            )
            .execute(&mut *self.tx)
            .await?;
        }

        self.tx.commit().await?;
        Ok(())
    }
}

fn status_str(status: &SessionStatus) -> String {
    format!("{status:?}").to_lowercase()
}

fn parse_status(status: &str) -> SessionStatus {
    match status {
        "pending" => SessionStatus::Pending,
        "running" => SessionStatus::Running,
        "completed" => SessionStatus::Completed,
        "failed" => SessionStatus::Failed,
        "cancelled" => SessionStatus::Cancelled,
        _ => SessionStatus::Failed,
    }
}
