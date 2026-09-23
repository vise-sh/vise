//! The routine linkage on sessions: a routine-spawned session is stamped with
//! its routine's id, `get` surfaces it, and `list_by_routine` scopes to both
//! the routine and the workspace. These tests need `DATABASE_URL`;
//! `sqlx::test` creates a throwaway database per test.

use chrono::Utc;
use sqlx::PgPool;
use vise_core::sessions::model::{Agent, Environment, Session, SessionStatus};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::repository::SessionRepository;
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

async fn create_session(
    pool: &PgPool,
    workspace: &WorkspaceId,
    routine_id: Option<&str>,
) -> Session {
    let now = Utc::now();
    PostgresSessionRepository::new(pool.clone())
        .create(Session {
            id: vise_core::id::new_id("ses"),
            workspace_id: workspace.clone(),
            agent: Agent {
                harness: "echo".into(),
                model: String::new(),
                instructions: String::new(),
                mcp_servers: vec![],
            },
            environment: Environment {
                kind: "self_hosted".into(),
                repo: None,
                base_branch: None,
            },
            input: "do the thing".into(),
            status: SessionStatus::Pending,
            host_id: None,
            lease_expires_at: None,
            started_at: None,
            finished_at: None,
            stop_reason: None,
            error: None,
            outcome: None,
            pr_status: None,
            parent_session_id: None,
            routine_id: routine_id.map(str::to_string),
            cancel_requested: false,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap()
}

#[sqlx::test(migrations = "./migrations")]
async fn session_carries_routine_id(pool: PgPool) {
    let repo = PostgresSessionRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_routines").await;

    let stamped = create_session(&pool, &ws, Some("rtn_test")).await;
    let fetched = repo.get(&ws, &stamped.id).await.unwrap().unwrap();
    assert_eq!(fetched.routine_id.as_deref(), Some("rtn_test"));

    let ad_hoc = create_session(&pool, &ws, None).await;
    let fetched = repo.get(&ws, &ad_hoc.id).await.unwrap().unwrap();
    assert_eq!(fetched.routine_id, None);
}

#[sqlx::test(migrations = "./migrations")]
async fn list_by_routine_scopes_to_routine_and_workspace(pool: PgPool) {
    let repo = PostgresSessionRepository::new(pool.clone());
    let ws = create_workspace(&pool, "ws_a").await;
    let other_ws = create_workspace(&pool, "ws_b").await;

    let a1 = create_session(&pool, &ws, Some("rtn_a")).await;
    let a2 = create_session(&pool, &ws, Some("rtn_a")).await;
    let _b = create_session(&pool, &ws, Some("rtn_b")).await;

    let mut got: Vec<String> = repo
        .list_by_routine(&ws, "rtn_a")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    got.sort();
    let mut expected = vec![a1.id, a2.id];
    expected.sort();
    assert_eq!(got, expected);

    // A session in another workspace sharing the same routine id must not leak
    // across the workspace boundary: it stays out of ws_a's list, and ws_b sees
    // only its own. This makes the `workspace_id` predicate load-bearing — the
    // routine-id match alone would otherwise pass this test.
    let cross = create_session(&pool, &other_ws, Some("rtn_a")).await;

    let mut still_a: Vec<String> = repo
        .list_by_routine(&ws, "rtn_a")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    still_a.sort();
    assert_eq!(still_a, expected);

    let got_b: Vec<String> = repo
        .list_by_routine(&other_ws, "rtn_a")
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(got_b, vec![cross.id]);
}
