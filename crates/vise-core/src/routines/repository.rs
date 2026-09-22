use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::model::Routine;
use crate::workspaces::model::WorkspaceId;

/// Routine storage.
///
/// Two kinds of caller use this trait, and they establish workspace scope
/// differently:
///
/// - **User-facing** methods (`create`, `get`, `list`, `update`, `delete`)
///   take an explicit [`WorkspaceId`] and never see routines outside it.
/// - **Server-side sweep** methods (`claim_due`, `record_fire`) run in the
///   scheduler across every workspace and take no [`WorkspaceId`]; they key
///   off the routine's schedule, not its owner.
#[async_trait]
pub trait RoutineRepository: Send + Sync {
    /// Insert a routine into the workspace named by `routine.workspace_id`.
    async fn create(&self, routine: Routine) -> anyhow::Result<Routine>;

    async fn get(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<Option<Routine>>;

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Routine>>;

    /// Update a routine identified by `(workspace_id, id)`. Sets `updated_at`
    /// to now. Returns `None` if no such routine exists in the workspace.
    /// Deliberately does not write the scheduler-owned fire metadata
    /// (`last_fired_at`, `last_session_id`) — those are owned by
    /// [`record_fire`](Self::record_fire) and a user-facing update must not
    /// clobber them.
    async fn update(&self, routine: Routine) -> anyhow::Result<Option<Routine>>;

    async fn delete(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<()>;

    /// Atomically claim up to `limit` enabled routines that are due at `now`,
    /// across ALL workspaces (this is a server-side sweep like session lease
    /// expiry). Uses `FOR UPDATE SKIP LOCKED` so concurrent schedulers /
    /// replicas never claim the same routine. Claiming provisionally pushes
    /// `next_run_at` forward by a short lease so a claimed routine is not
    /// re-claimed on the next tick before `record_fire` writes its true next
    /// occurrence; if the caller crashes between claim and record_fire, the
    /// routine simply re-fires after the lease. Returns the claimed routines
    /// (their in-memory `next_run_at` reflects the provisional lease).
    async fn claim_due(&self, now: DateTime<Utc>, limit: i64) -> anyhow::Result<Vec<Routine>>;

    /// Write the outcome of a fire (or an overlap-skip): set `next_run_at` to
    /// the true next occurrence, and update `last_fired_at` / `last_session_id`
    /// (pass the existing values unchanged on an overlap-skip). Always advances
    /// the schedule.
    async fn record_fire(
        &self,
        id: &str,
        next_run_at: DateTime<Utc>,
        last_fired_at: Option<DateTime<Utc>>,
        last_session_id: Option<String>,
    ) -> anyhow::Result<()>;
}
