//! The routine scheduler background task: one `run_once` pass claims the due
//! routines and spawns a session per due routine. The infinite `run_forever`
//! loop is a thin `tokio::interval` wrapper, so `run_once` is the unit under
//! test. Builds the full service stack against a throwaway database
//! (`sqlx::test` applies migrations 0001-… from vise-core).

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use sqlx::PgPool;
use vise_api::routine_scheduler::RoutineScheduler;
use vise_core::routines::model::SessionSpec;
use vise_core::routines::postgres::PostgresRoutineRepository;
use vise_core::routines::repository::RoutineRepository;
use vise_core::routines::service::RoutineService;
use vise_core::sessions::model::{Agent, Environment};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::service::SessionService;
use vise_core::workspaces::model::WorkspaceId;

/// A valid, `self_hosted` spec — needs no repo, so the tick can spawn a
/// session without touching GitHub.
fn valid_spec() -> SessionSpec {
    SessionSpec {
        agent: Agent {
            harness: "claude".into(),
            model: "opus".into(),
            instructions: "tidy up".into(),
            mcp_servers: vec![],
        },
        environment: Environment {
            kind: "self_hosted".into(),
            repo: None,
            base_branch: None,
        },
        input: "run the nightly".into(),
    }
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn scheduler_run_once_spawns_due_session(pool: PgPool) {
    let ws = WorkspaceId::DEFAULT;

    // Full stack, wired like vise-server: RoutineService over the Postgres
    // routine + session repos. Keep a bare repo handle to force the routine due.
    let sessions = Arc::new(SessionService::new(PostgresSessionRepository::new(
        pool.clone(),
    )));
    let routines_repo = PostgresRoutineRepository::new(pool.clone());
    let routines = Arc::new(RoutineService::new(
        PostgresRoutineRepository::new(pool.clone()),
        sessions.clone(),
    ));

    let routine = routines
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();

    // Force it due: push next_run_at into the past via record_fire (same
    // technique the routine_service unit tests use).
    routines_repo
        .record_fire(
            &routine.id,
            Utc::now() - ChronoDuration::minutes(1),
            None,
            None,
        )
        .await
        .unwrap();

    let scheduler = RoutineScheduler::new(routines.clone(), Duration::from_secs(60));

    let report = scheduler.run_once().await;
    assert_eq!(report.spawned, 1);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.failed, 0);

    // Exactly one session, stamped with the routine id.
    let spawned = sessions.list_by_routine(&ws, &routine.id).await.unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].routine_id.as_deref(), Some(routine.id.as_str()));
}
