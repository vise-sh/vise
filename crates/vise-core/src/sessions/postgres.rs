use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::{
    model::{
        Agent, Environment, NewSessionEvent, PrStatus, Session, SessionOutcome, SessionStatus,
    },
    repository::SessionRepository,
};

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
    cancel_requested: bool,
    parent_session_id: Option<String>,
    pr_status: Option<sqlx::types::Json<PrStatus>>,
    pr_sync_failures: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<SessionRow> for Session {
    fn from(row: SessionRow) -> Self {
        Session {
            id: row.id,
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
            cancel_requested: row.cancel_requested,
            parent_session_id: row.parent_session_id,
            pr_status: row.pr_status.map(|j| j.0),
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// A session claimed for one PR-tracking poll. Holds the row lock (inside an
/// open transaction) until it is recorded or released, so concurrent server
/// instances skip it via `FOR UPDATE SKIP LOCKED`.
pub struct PrTrackingClaim {
    tx: sqlx::Transaction<'static, sqlx::Postgres>,
    pub session: Session,
    /// Consecutive 401/403/404 polls before this one.
    pub sync_failures: i32,
}

impl PostgresSessionRepository {
    /// Lock the next session whose PR needs syncing: a `pr_opened` outcome,
    /// a non-terminal (or absent) snapshot, and a snapshot older than
    /// `synced_before`. `exclude` skips sessions already handled this tick
    /// (needed because a failed poll deliberately leaves `last_synced_at`
    /// untouched). Returns `None` when the work list is empty.
    pub async fn claim_pr_tracking(
        &self,
        synced_before: DateTime<Utc>,
        exclude: &[String],
    ) -> anyhow::Result<Option<PrTrackingClaim>> {
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
                created_at,
                updated_at
            FROM sessions
            WHERE outcome->>'kind' = 'pr_opened'
              AND outcome->>'pr_url' IS NOT NULL
              AND (pr_status IS NULL OR pr_status->>'state' NOT IN ('merged', 'closed'))
              AND (pr_status IS NULL OR (pr_status->>'last_synced_at')::timestamptz < $1)
              AND NOT (id = ANY($2))
            ORDER BY (pr_status->>'last_synced_at')::timestamptz ASC NULLS FIRST, created_at ASC
            LIMIT 1
            FOR UPDATE SKIP LOCKED
            "#,
            synced_before,
            exclude
        )
        .fetch_optional(&mut *tx)
        .await?;

        match row {
            Some(row) => {
                let sync_failures = row.pr_sync_failures;
                Ok(Some(PrTrackingClaim {
                    tx,
                    session: Session::from(row),
                    sync_failures,
                }))
            }
            None => {
                tx.rollback().await?;
                Ok(None)
            }
        }
    }

    /// Write the new snapshot and append its transition events in the claim's
    /// transaction, then commit. Resets the failure counter. Event `seq`
    /// continues after the session's last event; after `finished_at` the
    /// poller is the only writer, so the row lock is enough to serialize.
    pub async fn record_pr_status(
        &self,
        claim: PrTrackingClaim,
        status: &PrStatus,
        events: &[serde_json::Value],
    ) -> anyhow::Result<()> {
        let PrTrackingClaim {
            mut tx, session, ..
        } = claim;

        sqlx::query!(
            r#"
            UPDATE sessions
            SET pr_status = $2,
                pr_sync_failures = 0,
                updated_at = now()
            WHERE id = $1
            "#,
            session.id,
            sqlx::types::Json(status) as _
        )
        .execute(&mut *tx)
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
                session.id,
                events
            )
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Count a 401/403/404 poll against the session without touching the
    /// snapshot. The transaction stays open so the caller can either
    /// [`commit_pr_tracking`](Self::commit_pr_tracking) or, once the failure
    /// threshold is reached, write a `sync_error` snapshot atomically.
    pub async fn record_pr_sync_failure(
        &self,
        mut claim: PrTrackingClaim,
    ) -> anyhow::Result<PrTrackingClaim> {
        let failures = sqlx::query_scalar!(
            r#"
            UPDATE sessions
            SET pr_sync_failures = pr_sync_failures + 1,
                updated_at = now()
            WHERE id = $1
            RETURNING pr_sync_failures
            "#,
            claim.session.id
        )
        .fetch_one(&mut *claim.tx)
        .await?;

        claim.sync_failures = failures;
        Ok(claim)
    }

    /// Commit whatever the claim's transaction wrote and release the row.
    pub async fn commit_pr_tracking(&self, claim: PrTrackingClaim) -> anyhow::Result<()> {
        claim.tx.commit().await?;
        Ok(())
    }

    /// Give the row back untouched (transient failure, rate limit, shutdown).
    pub async fn release_pr_tracking(&self, claim: PrTrackingClaim) -> anyhow::Result<()> {
        claim.tx.rollback().await?;
        Ok(())
    }
}

#[async_trait]
impl SessionRepository for PostgresSessionRepository {
    async fn create(&self, session: Session) -> anyhow::Result<Session> {
        sqlx::query(
            r#"
            INSERT INTO sessions (
                id,
                agent,
                environment,
                input,
                status,
                parent_session_id,
                created_at,
                updated_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(&session.id)
        .bind(sqlx::types::Json(&session.agent))
        .bind(sqlx::types::Json(&session.environment))
        .bind(&session.input)
        .bind(status_str(&session.status))
        .bind(&session.parent_session_id)
        .bind(session.created_at)
        .bind(session.updated_at)
        .execute(&self.pool)
        .await?;

        Ok(session)
    }

    async fn get(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
                created_at,
                updated_at
            FROM sessions
            WHERE id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Session::from))
    }

    async fn list(&self) -> anyhow::Result<Vec<Session>> {
        let rows = sqlx::query_as!(
            SessionRow,
            r#"
            SELECT
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
                created_at,
                updated_at
            FROM sessions
            ORDER BY created_at DESC
            "#
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Session::from).collect())
    }

    async fn delete(&self, id: &str) -> anyhow::Result<()> {
        sqlx::query!(
            r#"
            DELETE FROM sessions
            WHERE id = $1
            "#,
            id
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn get_events(
        &self,
        id: &str,
        after_seq: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<super::model::SessionEvent>> {
        let rows = sqlx::query!(
            r#"
            SELECT session_id, seq, payload, created_at
            FROM session_events
            WHERE session_id = $1 AND seq > $2
            ORDER BY seq
            LIMIT $3
            "#,
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
                ORDER BY created_at
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
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
            RETURNING
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
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

    async fn request_cancel(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let row = sqlx::query_as!(
            SessionRow,
            r#"
            UPDATE sessions
            SET cancel_requested = true,
                status = CASE WHEN status = 'pending' THEN 'cancelled' ELSE status END,
                finished_at = CASE WHEN status = 'pending' THEN now() ELSE finished_at END,
                updated_at = now()
            WHERE id = $1 AND status IN ('pending', 'running')
            RETURNING
                id,
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
                cancel_requested,
                parent_session_id,
                pr_status as "pr_status: _",
                pr_sync_failures,
                created_at,
                updated_at
            "#,
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
