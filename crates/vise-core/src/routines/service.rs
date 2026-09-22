use std::sync::Arc;

use chrono::{DateTime, Utc};

use super::{
    model::{Routine, SessionSpec},
    repository::RoutineRepository,
    schedule,
};
use crate::sessions::model::{Session, SessionStatus};
use crate::sessions::repository::SessionRepository;
use crate::sessions::service::SessionService;
use crate::workspaces::model::WorkspaceId;

/// Most routines a single scheduler tick will claim and process. A cap keeps
/// one tick bounded; anything still due is picked up on the next tick.
const LIMIT: i64 = 100;

/// The result of one [`RoutineService::tick`] pass.
#[derive(Debug, Default, Clone, Copy)]
pub struct TickReport {
    /// Routines whose fire spawned a fresh session this tick.
    pub spawned: usize,
    /// Routines that were due but skipped because a prior run was still active.
    pub skipped: usize,
}

/// The logic layer over routine storage: validates routines on write and, on
/// each scheduler tick, turns the repository's claim into a fire decision
/// (spawn, or skip on overlap) and advances the schedule.
pub struct RoutineService<R, S> {
    routines: R,
    sessions: Arc<SessionService<S>>,
}

impl<R, S> RoutineService<R, S>
where
    R: RoutineRepository,
    S: SessionRepository,
{
    pub fn new(routines: R, sessions: Arc<SessionService<S>>) -> Self {
        Self { routines, sessions }
    }

    /// Validate a routine and persist it with its first `next_run_at` computed.
    /// The three validations run in a fixed order and the first failure is
    /// returned; the API layer maps these to 422.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        name: String,
        cron: String,
        timezone: String,
        spec: SessionSpec,
    ) -> anyhow::Result<Routine> {
        spec.environment
            .validate()
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        schedule::validate_min_interval(&cron)?;
        let now = Utc::now();
        // Also validates that `cron` parses and `timezone` is a real IANA zone.
        let next_run_at = schedule::next_after(&cron, &timezone, now)?;

        let routine = Routine {
            id: crate::id::new_id("rtn"),
            workspace_id: workspace,
            name,
            cron,
            timezone,
            spec,
            enabled: true,
            next_run_at,
            last_fired_at: None,
            last_session_id: None,
            created_at: now,
            updated_at: now,
        };

        self.routines.create(routine).await
    }

    pub async fn get(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<Option<Routine>> {
        self.routines.get(workspace, id).await
    }

    pub async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Routine>> {
        self.routines.list(workspace).await
    }

    pub async fn delete(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<()> {
        self.routines.delete(workspace, id).await
    }

    /// PATCH a routine: apply whichever fields are `Some`, leaving the rest
    /// untouched. If the schedule (cron or timezone) changed, re-validate and
    /// recompute `next_run_at`; if a new spec is given, validate its
    /// environment. Returns `Ok(None)` if the routine does not exist.
    // One `Option` per PATCHable field is the natural shape for partial-update
    // semantics; grouping them into a struct would only move the arity elsewhere.
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        workspace: &WorkspaceId,
        id: &str,
        name: Option<String>,
        cron: Option<String>,
        timezone: Option<String>,
        spec: Option<SessionSpec>,
        enabled: Option<bool>,
    ) -> anyhow::Result<Option<Routine>> {
        let Some(mut routine) = self.routines.get(workspace, id).await? else {
            return Ok(None);
        };

        let schedule_changed = cron.is_some() || timezone.is_some();

        if let Some(name) = name {
            routine.name = name;
        }
        if let Some(cron) = cron {
            routine.cron = cron;
        }
        if let Some(timezone) = timezone {
            routine.timezone = timezone;
        }
        if let Some(spec) = spec {
            spec.environment
                .validate()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            routine.spec = spec;
        }
        if let Some(enabled) = enabled {
            routine.enabled = enabled;
        }

        if schedule_changed {
            schedule::validate_min_interval(&routine.cron)?;
            routine.next_run_at =
                schedule::next_after(&routine.cron, &routine.timezone, Utc::now())?;
        }

        self.routines.update(routine).await
    }

    /// Spawn a session for a routine off-schedule, immediately. Bypasses the
    /// overlap rule and does NOT advance `next_run_at` — a manual run is not a
    /// scheduled fire. Returns `Ok(None)` if the routine does not exist.
    pub async fn run_now(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<Session>> {
        let Some(routine) = self.routines.get(workspace, id).await? else {
            return Ok(None);
        };

        let session = self
            .sessions
            .create(
                routine.workspace_id,
                routine.spec.agent,
                routine.spec.environment,
                routine.spec.input,
                None,
                Some(routine.id),
            )
            .await?;

        Ok(Some(session))
    }

    /// One scheduler pass: claim the due routines, and for each decide whether
    /// to fire. `next_run_at` always advances from `now` (missed ticks are
    /// skipped, never backfilled). A routine with a still-active prior run is
    /// skipped this tick but its schedule still advances.
    pub async fn tick(&self, now: DateTime<Utc>) -> anyhow::Result<TickReport> {
        let due = self.routines.claim_due(now, LIMIT).await?;

        let mut report = TickReport::default();

        for routine in due {
            let next = schedule::next_after(&routine.cron, &routine.timezone, now)?;

            let active = self
                .sessions
                .list_by_routine(&routine.workspace_id, &routine.id)
                .await?
                .iter()
                .any(|s| matches!(s.status, SessionStatus::Pending | SessionStatus::Running));

            if active {
                self.routines
                    .record_fire(
                        &routine.id,
                        next,
                        routine.last_fired_at,
                        routine.last_session_id.clone(),
                    )
                    .await?;
                report.skipped += 1;
                tracing::info!(routine = %routine.id, "skipped_overlap");
            } else {
                let session = self
                    .sessions
                    .create(
                        routine.workspace_id.clone(),
                        routine.spec.agent.clone(),
                        routine.spec.environment.clone(),
                        routine.spec.input.clone(),
                        None,
                        Some(routine.id.clone()),
                    )
                    .await?;
                self.routines
                    .record_fire(&routine.id, next, Some(now), Some(session.id))
                    .await?;
                report.spawned += 1;
            }
        }

        if report.spawned > 0 || report.skipped > 0 {
            tracing::info!(
                spawned = report.spawned,
                skipped = report.skipped,
                "routine tick"
            );
        }

        Ok(report)
    }
}
