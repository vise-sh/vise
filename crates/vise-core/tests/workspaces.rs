//! Cross-workspace isolation at the repository layer. Every user-facing
//! method takes a `WorkspaceId` and must not see rows outside it; every
//! host-driven method derives its scope from the host row and must not let a
//! host touch another workspace's sessions. These tests need `DATABASE_URL`;
//! `sqlx::test` creates a throwaway database per test.

use chrono::Utc;
use sqlx::PgPool;
use vise_core::hosts::model::Host;
use vise_core::hosts::postgres::PostgresHostRepository;
use vise_core::hosts::repository::HostRepository;
use vise_core::sessions::model::{Agent, Environment, NewSessionEvent, Session, SessionStatus};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::repository::SessionRepository;
use vise_core::workspaces::model::{Workspace, WorkspaceId};
use vise_core::workspaces::postgres::PostgresWorkspaceRepository;
use vise_core::workspaces::repository::WorkspaceRepository;

fn ws(id: &str) -> WorkspaceId {
    WorkspaceId::new(id)
}

async fn create_workspace(pool: &PgPool, id: &str) -> WorkspaceId {
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(Workspace {
            id: ws(id),
            name: id.to_string(),
            settings: serde_json::json!({}),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    ws(id)
}

async fn enroll(pool: &PgPool, workspace: &WorkspaceId, name: &str) -> anyhow::Result<Host> {
    let hosts = PostgresHostRepository::new(pool.clone());
    let host = Host {
        id: vise_core::id::new_id("host"),
        workspace_id: workspace.clone(),
        name: name.to_string(),
        last_seen_at: None,
        created_at: Utc::now(),
    };
    let token_hash = vise_core::id::new_token("hash");
    hosts.create(host, &token_hash).await
}

fn new_session(workspace: &WorkspaceId) -> Session {
    let now = Utc::now();
    Session {
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
        cancel_requested: false,
        created_at: now,
        updated_at: now,
    }
}

async fn create_session(pool: &PgPool, workspace: &WorkspaceId) -> Session {
    PostgresSessionRepository::new(pool.clone())
        .create(new_session(workspace))
        .await
        .unwrap()
}

fn event(seq: i64) -> NewSessionEvent {
    NewSessionEvent {
        seq,
        payload: serde_json::json!({ "seq": seq }),
    }
}

// --- workspaces -------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn default_workspace_is_seeded(pool: PgPool) {
    let workspaces = PostgresWorkspaceRepository::new(pool);

    let default = workspaces
        .get(&WorkspaceId::DEFAULT)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(default.id, WorkspaceId::DEFAULT);
    assert_eq!(default.settings, serde_json::json!({}));

    let all = workspaces.list().await.unwrap();
    assert_eq!(all.len(), 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn workspace_repository_round_trips(pool: PgPool) {
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());

    let created = workspaces
        .create(Workspace {
            id: ws("ws_acme"),
            name: "Acme".into(),
            settings: serde_json::json!({ "plan": "team" }),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    let fetched = workspaces.get(&created.id).await.unwrap().unwrap();
    assert_eq!(fetched.name, "Acme");
    assert_eq!(fetched.settings, serde_json::json!({ "plan": "team" }));
    assert_eq!(workspaces.list().await.unwrap().len(), 2);

    workspaces.delete(&created.id).await.unwrap();
    assert!(workspaces.get(&created.id).await.unwrap().is_none());
    assert!(workspaces.get(&ws("nope")).await.unwrap().is_none());
}

#[sqlx::test(migrations = "./migrations")]
async fn workspace_with_hosts_cannot_be_deleted(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    enroll(&pool, &acme, "box").await.unwrap();

    let workspaces = PostgresWorkspaceRepository::new(pool);
    assert!(workspaces.delete(&acme).await.is_err());
}

// --- hosts ----------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn host_names_are_unique_per_workspace_not_globally(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;

    enroll(&pool, &acme, "build-box").await.unwrap();
    enroll(&pool, &globex, "build-box")
        .await
        .expect("same name in another workspace is allowed");
    assert!(
        enroll(&pool, &acme, "build-box").await.is_err(),
        "same name in the same workspace must collide"
    );

    let hosts = PostgresHostRepository::new(pool);
    let acme_hosts = hosts.list(&acme).await.unwrap();
    assert_eq!(acme_hosts.len(), 1);
    assert_eq!(acme_hosts[0].workspace_id, acme);
    assert_eq!(hosts.list(&globex).await.unwrap().len(), 1);
    assert!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn host_must_belong_to_an_existing_workspace(pool: PgPool) {
    assert!(enroll(&pool, &ws("ws_missing"), "box").await.is_err());
}

// --- user-facing session methods --------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reads_lists_and_deletes_do_not_cross_workspaces(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    let a = create_session(&pool, &acme).await;
    let b = create_session(&pool, &globex).await;
    let in_default = create_session(&pool, &WorkspaceId::DEFAULT).await;

    // get
    assert_eq!(sessions.get(&acme, &a.id).await.unwrap().unwrap().id, a.id);
    assert!(sessions.get(&globex, &a.id).await.unwrap().is_none());
    assert!(
        sessions
            .get(&WorkspaceId::DEFAULT, &a.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sessions
            .get(&WorkspaceId::DEFAULT, &in_default.id)
            .await
            .unwrap()
            .unwrap()
            .workspace_id,
        WorkspaceId::DEFAULT
    );

    // list
    let acme_list = sessions.list(&acme).await.unwrap();
    assert_eq!(acme_list.len(), 1);
    assert_eq!(acme_list[0].id, a.id);
    let globex_list = sessions.list(&globex).await.unwrap();
    assert_eq!(globex_list.len(), 1);
    assert_eq!(globex_list[0].id, b.id);
    assert_eq!(sessions.list(&WorkspaceId::DEFAULT).await.unwrap().len(), 1);

    // delete from the wrong workspace is a silent no-op
    sessions.delete(&globex, &a.id).await.unwrap();
    assert!(sessions.get(&acme, &a.id).await.unwrap().is_some());
    sessions.delete(&acme, &a.id).await.unwrap();
    assert!(sessions.get(&acme, &a.id).await.unwrap().is_none());
    assert!(sessions.get(&globex, &b.id).await.unwrap().is_some());
}

#[sqlx::test(migrations = "./migrations")]
async fn create_rejects_an_unknown_workspace(pool: PgPool) {
    let sessions = PostgresSessionRepository::new(pool);
    assert!(
        sessions
            .create(new_session(&ws("ws_missing")))
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn cancel_does_not_cross_workspaces(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    let a = create_session(&pool, &acme).await;

    assert!(
        sessions
            .request_cancel(&globex, &a.id)
            .await
            .unwrap()
            .is_none()
    );
    let still_pending = sessions.get(&acme, &a.id).await.unwrap().unwrap();
    assert!(matches!(still_pending.status, SessionStatus::Pending));
    assert!(!still_pending.cancel_requested);

    let cancelled = sessions
        .request_cancel(&acme, &a.id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(cancelled.status, SessionStatus::Cancelled));
}

#[sqlx::test(migrations = "./migrations")]
async fn events_do_not_cross_workspaces(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    let host = enroll(&pool, &acme, "box").await.unwrap();
    let a = create_session(&pool, &acme).await;
    let claimed = sessions.claim_pending(&host.id).await.unwrap().unwrap();
    assert_eq!(claimed.id, a.id);
    sessions
        .append_events(&host.id, &a.id, &[event(1), event(2)])
        .await
        .unwrap()
        .unwrap();

    let visible = sessions.get_events(&acme, &a.id, 0, 100).await.unwrap();
    assert_eq!(visible.len(), 2);
    assert!(
        sessions
            .get_events(&globex, &a.id, 0, 100)
            .await
            .unwrap()
            .is_empty(),
        "events must not be readable through another workspace"
    );
}

// --- host-driven session methods --------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn a_host_only_claims_pending_sessions_in_its_own_workspace(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    // Older pending work in globex must not tempt an acme host.
    let b = create_session(&pool, &globex).await;
    let acme_host = enroll(&pool, &acme, "acme-box").await.unwrap();

    assert!(
        sessions
            .claim_pending(&acme_host.id)
            .await
            .unwrap()
            .is_none(),
        "nothing pending in acme yet; globex work must stay untouched"
    );

    let a = create_session(&pool, &acme).await;
    let claimed = sessions
        .claim_pending(&acme_host.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, a.id);
    assert_eq!(claimed.workspace_id, acme);
    assert_eq!(claimed.host_id.as_deref(), Some(acme_host.id.as_str()));

    // globex's session is still pending and claimable by a globex host.
    let still_pending = sessions.get(&globex, &b.id).await.unwrap().unwrap();
    assert!(matches!(still_pending.status, SessionStatus::Pending));
    let globex_host = enroll(&pool, &globex, "globex-box").await.unwrap();
    let claimed = sessions
        .claim_pending(&globex_host.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, b.id);
}

#[sqlx::test(migrations = "./migrations")]
async fn an_unknown_host_claims_nothing(pool: PgPool) {
    create_session(&pool, &WorkspaceId::DEFAULT).await;
    let sessions = PostgresSessionRepository::new(pool);
    assert!(
        sessions
            .claim_pending("host_missing")
            .await
            .unwrap()
            .is_none()
    );
}

/// Even if a session somehow records a host from another workspace as its
/// holder, the host-driven writes re-derive scope from the host row and
/// refuse it. Simulates a corrupted or hand-edited `host_id`.
#[sqlx::test(migrations = "./migrations")]
async fn host_driven_writes_derive_scope_from_the_host_row(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let globex = create_workspace(&pool, "ws_globex").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    let globex_host = enroll(&pool, &globex, "globex-box").await.unwrap();
    let a = create_session(&pool, &acme).await;
    sqlx::query("UPDATE sessions SET status = 'running', host_id = $2 WHERE id = $1")
        .bind(&a.id)
        .bind(&globex_host.id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(
        sessions
            .heartbeat(&globex_host.id, &a.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .append_events(&globex_host.id, &a.id, &[event(1)])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        sessions
            .finish(
                &globex_host.id,
                &a.id,
                SessionStatus::Completed,
                None,
                None,
                None
            )
            .await
            .unwrap()
            .is_none()
    );

    let untouched = sessions.get(&acme, &a.id).await.unwrap().unwrap();
    assert!(matches!(untouched.status, SessionStatus::Running));
    assert!(
        sessions
            .get_events(&acme, &a.id, 0, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn a_host_drives_its_own_session_to_completion(pool: PgPool) {
    let acme = create_workspace(&pool, "ws_acme").await;
    let sessions = PostgresSessionRepository::new(pool.clone());

    let host = enroll(&pool, &acme, "box").await.unwrap();
    let a = create_session(&pool, &acme).await;
    sessions.claim_pending(&host.id).await.unwrap().unwrap();

    assert_eq!(
        sessions.heartbeat(&host.id, &a.id).await.unwrap(),
        Some(false)
    );
    sessions
        .append_events(&host.id, &a.id, &[event(1)])
        .await
        .unwrap()
        .unwrap();
    let finished = sessions
        .finish(
            &host.id,
            &a.id,
            SessionStatus::Completed,
            Some("end_turn".into()),
            None,
            None,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(finished.status, SessionStatus::Completed));
    assert_eq!(finished.workspace_id, acme);
}
