//! Routine persistence: CRUD roundtrips (spec JSONB intact), workspace scoping,
//! and the scheduler's server-side sweep (`claim_due` atomic claim+lease and
//! `record_fire`). These tests need `DATABASE_URL`; `sqlx::test` creates a
//! throwaway database per test.

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use vise_core::routines::model::{Routine, SessionSpec};
use vise_core::routines::postgres::PostgresRoutineRepository;
use vise_core::routines::repository::RoutineRepository;
use vise_core::sessions::model::{Agent, Environment};
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

fn build_routine(workspace: &WorkspaceId, next_run_at: DateTime<Utc>, enabled: bool) -> Routine {
    let now = Utc::now();
    Routine {
        id: vise_core::id::new_id("rtn"),
        workspace_id: workspace.clone(),
        name: "nightly".into(),
        cron: "0 9 * * *".into(),
        timezone: "America/New_York".into(),
        spec: SessionSpec {
            agent: Agent {
                harness: "claude".into(),
                model: "opus".into(),
                instructions: "tidy up".into(),
                mcp_servers: vec!["linear".into()],
            },
            environment: Environment {
                kind: "github_repo".into(),
                repo: Some("vise-sh/vise".into()),
                base_branch: Some("main".into()),
            },
            input: "run the nightly".into(),
        },
        enabled,
        next_run_at,
        last_fired_at: None,
        last_session_id: None,
        created_at: now,
        updated_at: now,
    }
}

#[sqlx::test(migrations = "./migrations")]
async fn create_then_get_roundtrips(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;

    let routine = build_routine(&ws, Utc::now(), true);
    let created = repo.create(routine.clone()).await.unwrap();
    assert_eq!(created.id, routine.id);

    let fetched = repo.get(&ws, &routine.id).await.unwrap().unwrap();
    assert_eq!(fetched.id, routine.id);
    assert_eq!(fetched.name, "nightly");
    assert_eq!(fetched.cron, "0 9 * * *");
    assert_eq!(fetched.timezone, "America/New_York");
    assert!(fetched.enabled);
    // The spec JSONB survives the roundtrip intact.
    assert_eq!(fetched.spec.agent.harness, "claude");
    assert_eq!(fetched.spec.agent.model, "opus");
    assert_eq!(fetched.spec.agent.mcp_servers, vec!["linear".to_string()]);
    assert_eq!(fetched.spec.environment.kind, "github_repo");
    assert_eq!(fetched.spec.environment.repo.as_deref(), Some("vise-sh/vise"));
    assert_eq!(fetched.spec.environment.base_branch.as_deref(), Some("main"));
    assert_eq!(fetched.spec.input, "run the nightly");
}

#[sqlx::test(migrations = "./migrations")]
async fn list_scoped_to_workspace(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws_a = create_workspace(&pool, "ws_a").await;
    let ws_b = create_workspace(&pool, "ws_b").await;

    let in_a = repo
        .create(build_routine(&ws_a, Utc::now(), true))
        .await
        .unwrap();

    let a_list = repo.list(&ws_a).await.unwrap();
    assert_eq!(a_list.len(), 1);
    assert_eq!(a_list[0].id, in_a.id);

    let b_list = repo.list(&ws_b).await.unwrap();
    assert!(b_list.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn update_changes_fields_and_bumps_updated_at(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;

    let created = repo
        .create(build_routine(&ws, Utc::now(), true))
        .await
        .unwrap();

    let new_next = Utc::now() + Duration::hours(6);
    let mut edited = created.clone();
    edited.name = "weekly".into();
    edited.cron = "0 9 * * 1".into();
    edited.enabled = false;
    edited.next_run_at = new_next;

    let updated = repo.update(edited).await.unwrap().unwrap();
    assert_eq!(updated.name, "weekly");
    assert!(updated.updated_at >= created.updated_at);

    let fetched = repo.get(&ws, &created.id).await.unwrap().unwrap();
    assert_eq!(fetched.name, "weekly");
    assert_eq!(fetched.cron, "0 9 * * 1");
    assert!(!fetched.enabled);
    assert_eq!(fetched.next_run_at.timestamp(), new_next.timestamp());

    // Updating a routine that does not exist returns None.
    let mut missing = created.clone();
    missing.id = "rtn_nope".into();
    assert!(repo.update(missing).await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn delete_removes(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;

    let created = repo
        .create(build_routine(&ws, Utc::now(), true))
        .await
        .unwrap();

    repo.delete(&ws, &created.id).await.unwrap();
    assert!(repo.get(&ws, &created.id).await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_due_returns_only_enabled_and_due(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;
    let now = Utc::now();

    let due_enabled = repo
        .create(build_routine(&ws, now - Duration::minutes(1), true))
        .await
        .unwrap();
    let _due_disabled = repo
        .create(build_routine(&ws, now - Duration::minutes(1), false))
        .await
        .unwrap();
    let _future_enabled = repo
        .create(build_routine(&ws, now + Duration::hours(1), true))
        .await
        .unwrap();

    let claimed = repo.claim_due(now, 10).await.unwrap();
    let ids: Vec<String> = claimed.into_iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![due_enabled.id]);
}

#[sqlx::test(migrations = "./migrations")]
async fn claim_due_leases_so_second_claim_is_empty(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;
    let now = Utc::now();

    let due = repo
        .create(build_routine(&ws, now - Duration::minutes(1), true))
        .await
        .unwrap();

    let first = repo.claim_due(now, 10).await.unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].id, due.id);
    // The claim pushed next_run_at forward by the 5-minute lease. Pin the
    // interval so a future edit to the lease can't silently drift it.
    assert!(first[0].next_run_at > now + Duration::minutes(4));
    assert!(first[0].next_run_at < now + Duration::minutes(6));

    // An immediate second claim at the same instant finds nothing to do: the
    // lease moved the routine out of the due window.
    let second = repo.claim_due(now, 10).await.unwrap();
    assert!(second.is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn record_fire_advances_and_sets_metadata(pool: PgPool) {
    let repo = PostgresRoutineRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_rtn").await;
    let now = Utc::now();

    let created = repo
        .create(build_routine(&ws, now, true))
        .await
        .unwrap();

    let next = now + Duration::hours(24);
    repo.record_fire(&created.id, next, Some(now), Some("ses_abc".into()))
        .await
        .unwrap();

    let fetched = repo.get(&ws, &created.id).await.unwrap().unwrap();
    assert_eq!(fetched.next_run_at.timestamp(), next.timestamp());
    assert_eq!(
        fetched.last_fired_at.map(|t| t.timestamp()),
        Some(now.timestamp())
    );
    assert_eq!(fetched.last_session_id.as_deref(), Some("ses_abc"));
}
