//! The routine logic layer: validation on create/update, the scheduler's fire
//! decision (`tick`) including the overlap rule and schedule advance, and the
//! off-schedule `run_now` spawn. These tests build the full stack against a
//! throwaway database (`sqlx::test` applies migrations 0001-0004).

use std::sync::Arc;

use chrono::{Duration, Utc};
use sqlx::PgPool;
use vise_core::routines::model::{SessionSpec, UpdateRoutine};
use vise_core::routines::postgres::PostgresRoutineRepository;
use vise_core::routines::repository::RoutineRepository;
use vise_core::routines::service::RoutineService;
use vise_core::sessions::model::{Agent, Environment};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::service::SessionService;
use vise_core::workspaces::model::{Workspace, WorkspaceId};
use vise_core::workspaces::postgres::PostgresWorkspaceRepository;
use vise_core::workspaces::repository::WorkspaceRepository;

async fn create_workspace(pool: &PgPool, id: &str) -> WorkspaceId {
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(Workspace {
            id: WorkspaceId::new(id),
            name: id.to_string(),
            settings: serde_json::json!({}),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    WorkspaceId::new(id)
}

/// A valid, `self_hosted` spec — needs no repo, so `tick`/`run_now` can spawn a
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

fn build_service(
    pool: &PgPool,
) -> (
    RoutineService<PostgresRoutineRepository, PostgresSessionRepository>,
    Arc<SessionService<PostgresSessionRepository>>,
    PostgresRoutineRepository,
) {
    let sessions = Arc::new(SessionService::new(PostgresSessionRepository::new(
        pool.clone(),
    )));
    let routines = PostgresRoutineRepository::new(pool.clone());
    let service = RoutineService::new(
        PostgresRoutineRepository::new(pool.clone()),
        sessions.clone(),
    );
    (service, sessions, routines)
}

#[sqlx::test(migrations = "./migrations")]
async fn create_rejects_bad_schedule_and_spec(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, _sessions, _routines) = build_service(&pool);

    // Sub-15-minute schedules are rejected.
    assert!(
        service
            .create(
                ws.clone(),
                "n".into(),
                "*/5 * * * *".into(),
                "UTC".into(),
                valid_spec(),
            )
            .await
            .is_err()
    );

    // Unknown timezone is rejected.
    assert!(
        service
            .create(
                ws.clone(),
                "n".into(),
                "0 9 * * *".into(),
                "Mars/Phobos".into(),
                valid_spec(),
            )
            .await
            .is_err()
    );

    // A github_repo spec with no repo is rejected.
    let mut bad_spec = valid_spec();
    bad_spec.environment = Environment {
        kind: "github_repo".into(),
        repo: None,
        base_branch: None,
    };
    assert!(
        service
            .create(
                ws.clone(),
                "n".into(),
                "0 9 * * *".into(),
                "UTC".into(),
                bad_spec,
            )
            .await
            .is_err()
    );

    // A valid routine succeeds and its first next_run_at is in the future.
    let now = Utc::now();
    let routine = service
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    assert!(routine.next_run_at > now);
    assert!(routine.enabled);
    assert!(routine.last_fired_at.is_none());
    assert!(routine.last_session_id.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn tick_spawns_due_session_stamped_with_routine_id(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, sessions, routines) = build_service(&pool);

    let routine = service
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();

    // Force it due: push next_run_at into the past via record_fire.
    routines
        .record_fire(&routine.id, Utc::now() - Duration::minutes(1), None, None)
        .await
        .unwrap();

    let now = Utc::now();
    let report = service.tick(now).await.unwrap();
    assert_eq!(report.spawned, 1);
    assert_eq!(report.skipped, 0);

    let spawned = sessions.list_by_routine(&ws, &routine.id).await.unwrap();
    assert_eq!(spawned.len(), 1);
    assert_eq!(spawned[0].routine_id.as_deref(), Some(routine.id.as_str()));

    let fetched = service.get(&ws, &routine.id).await.unwrap().unwrap();
    assert_eq!(
        fetched.last_session_id.as_deref(),
        Some(spawned[0].id.as_str())
    );
    assert!(fetched.next_run_at > now);
}

#[sqlx::test(migrations = "./migrations")]
async fn tick_skips_when_prior_run_active(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, sessions, routines) = build_service(&pool);

    let routine = service
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();

    // Pre-seed a Pending session stamped with the routine's id — a still-active
    // prior run.
    sessions
        .create(
            ws.clone(),
            valid_spec().agent,
            valid_spec().environment,
            valid_spec().input,
            None,
            Some(routine.id.clone()),
        )
        .await
        .unwrap();

    routines
        .record_fire(&routine.id, Utc::now() - Duration::minutes(1), None, None)
        .await
        .unwrap();

    let now = Utc::now();
    let report = service.tick(now).await.unwrap();
    assert_eq!(report.skipped, 1);
    assert_eq!(report.spawned, 0);

    // No NEW session was created; only the pre-seeded one exists.
    let all = sessions.list_by_routine(&ws, &routine.id).await.unwrap();
    assert_eq!(all.len(), 1);

    // The schedule still advanced.
    let fetched = service.get(&ws, &routine.id).await.unwrap().unwrap();
    assert!(fetched.next_run_at > now);
}

#[sqlx::test(migrations = "./migrations")]
async fn tick_ignores_future_and_disabled(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, _sessions, routines) = build_service(&pool);

    // A future routine (default next_run_at is in the future).
    let _future = service
        .create(
            ws.clone(),
            "future".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();

    // A disabled routine that is due — must still not fire.
    let disabled = service
        .create(
            ws.clone(),
            "disabled".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    service
        .update(
            &ws,
            &disabled.id,
            UpdateRoutine {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    routines
        .record_fire(&disabled.id, Utc::now() - Duration::minutes(1), None, None)
        .await
        .unwrap();

    let report = service.tick(Utc::now()).await.unwrap();
    assert_eq!(report.spawned, 0);
    assert_eq!(report.skipped, 0);
}

#[sqlx::test(migrations = "./migrations")]
async fn run_now_spawns_despite_active_run_and_leaves_next_run_at(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, sessions, _routines) = build_service(&pool);

    let routine = service
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    let next_before = routine.next_run_at;

    // Pre-seed an active run — run_now must bypass the overlap rule.
    sessions
        .create(
            ws.clone(),
            valid_spec().agent,
            valid_spec().environment,
            valid_spec().input,
            None,
            Some(routine.id.clone()),
        )
        .await
        .unwrap();

    let session = service.run_now(&ws, &routine.id).await.unwrap();
    assert!(session.is_some());

    let all = sessions.list_by_routine(&ws, &routine.id).await.unwrap();
    assert_eq!(all.len(), 2);

    // next_run_at is unchanged: a manual run is not a scheduled fire.
    let fetched = service.get(&ws, &routine.id).await.unwrap().unwrap();
    assert_eq!(fetched.next_run_at.timestamp(), next_before.timestamp());
    assert!(fetched.last_session_id.is_none());

    // A missing routine yields None.
    assert!(service.run_now(&ws, "rtn_nope").await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn update_recomputes_next_run_at_when_cron_changes(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, _sessions, _routines) = build_service(&pool);

    let routine = service
        .create(
            ws.clone(),
            "nightly".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    let next_before = routine.next_run_at;

    // Change cron to a different valid schedule (weekly Monday 09:00).
    let updated = service
        .update(
            &ws,
            &routine.id,
            UpdateRoutine {
                cron: Some("0 9 * * 1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.cron, "0 9 * * 1");
    assert_ne!(updated.next_run_at.timestamp(), next_before.timestamp());

    // Updating a missing id returns None.
    let missing = service
        .update(
            &ws,
            "rtn_nope",
            UpdateRoutine {
                name: Some("weekly".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(missing.is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn tick_isolates_a_poison_routine(pool: PgPool) {
    let ws = create_workspace(&pool, "ws_rtn").await;
    let (service, sessions, routines) = build_service(&pool);

    // A healthy, due routine.
    let healthy = service
        .create(
            ws.clone(),
            "healthy".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    routines
        .record_fire(&healthy.id, Utc::now() - Duration::minutes(1), None, None)
        .await
        .unwrap();

    // A poison routine: created valid (so it passes create-time validation),
    // then corrupted at the storage layer to an unparseable cron so `next_after`
    // fails when `tick` handles it. Also forced due via record_fire.
    let poison = service
        .create(
            ws.clone(),
            "poison".into(),
            "0 9 * * *".into(),
            "America/New_York".into(),
            valid_spec(),
        )
        .await
        .unwrap();
    sqlx::query("UPDATE routines SET cron = 'not a cron' WHERE id = $1")
        .bind(&poison.id)
        .execute(&pool)
        .await
        .unwrap();
    routines
        .record_fire(&poison.id, Utc::now() - Duration::minutes(1), None, None)
        .await
        .unwrap();

    let report = service.tick(Utc::now()).await.unwrap();
    // The poison row failed in isolation; the healthy routine still fired.
    assert_eq!(report.failed, 1);
    assert_eq!(report.spawned, 1);
    assert_eq!(report.skipped, 0);

    // The healthy routine actually spawned a session; the poison one did not.
    let healthy_sessions = sessions.list_by_routine(&ws, &healthy.id).await.unwrap();
    assert_eq!(healthy_sessions.len(), 1);
    let poison_sessions = sessions.list_by_routine(&ws, &poison.id).await.unwrap();
    assert_eq!(poison_sessions.len(), 0);
}
