mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use common::*;
use sqlx::PgPool;
use tower::ServiceExt;
use vise_core::sessions::model::{PrState, PrStatus, Session, SessionOutcome};
use vise_core::workspaces::model::WorkspaceId;
use wiremock::MockServer;

async fn follow_up(
    state: &vise_api::AppState,
    id: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = vise_api::app(state.clone())
        .oneshot(
            Request::post(format!("/sessions/{id}/follow-up"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn set_snapshot(pool: &PgPool, id: &str, state: PrState) {
    let status = PrStatus {
        state,
        checks: None,
        last_synced_at: chrono::Utc::now(),
    };
    sqlx::query("UPDATE sessions SET pr_status = $2 WHERE id = $1")
        .bind(id)
        .bind(sqlx::types::Json(status))
        .execute(pool)
        .await
        .unwrap();
}

async fn mount_feedback(server: &MockServer, number: u64) {
    mount_pull(
        server,
        number,
        pull_json(true, false, "vise/feature", "sha1"),
    )
    .await;
    mount_reviews(
        server,
        number,
        serde_json::json!([{
            "user": { "login": "alice" },
            "state": "CHANGES_REQUESTED",
            "body": "Please split this function up.",
            "submitted_at": "2026-09-12T10:01:00Z",
            "commit_id": "sha1"
        }]),
    )
    .await;
    mount_review_comments(
        server,
        number,
        serde_json::json!([
            {
                "id": 1, "in_reply_to_id": null, "user": { "login": "alice" },
                "path": "src/lib.rs", "line": 42, "original_line": 40,
                "body": "rename this to `reduce`", "created_at": "2026-09-12T10:00:00Z"
            },
            {
                "id": 2, "in_reply_to_id": 1, "user": { "login": "bob" },
                "path": "src/lib.rs", "line": 42, "original_line": 40,
                "body": "+1", "created_at": "2026-09-12T10:02:00Z"
            }
        ]),
    )
    .await;
    mount_check_runs(
        server,
        "sha1",
        serde_json::json!([
            check_run("clippy", "completed", Some("failure")),
            check_run("test", "completed", Some("success")),
            check_run("docs", "queued", None),
        ]),
    )
    .await;
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn composes_review_feedback_and_targets_the_head_branch(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let root = pr_session(&state, 17).await;
    mount_feedback(&server, 17).await;

    let (status, body) = follow_up(
        &state,
        &root.id,
        serde_json::json!({ "instructions": "Keep the public API stable." }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let created: Session = serde_json::from_value(body).unwrap();

    assert_eq!(created.parent_session_id.as_deref(), Some(root.id.as_str()));
    assert_eq!(created.environment.kind, "github_repo");
    assert_eq!(created.environment.repo.as_deref(), Some("acme/widgets"));
    assert_eq!(
        created.environment.base_branch.as_deref(),
        Some("vise/feature"),
        "follow-up pushes to the PR's head branch"
    );

    // Agent config inherited from the parent.
    assert_eq!(created.agent.harness, root.agent.harness);
    assert_eq!(created.agent.model, root.agent.model);
    assert_eq!(created.agent.instructions, root.agent.instructions);
    assert_eq!(created.agent.mcp_servers, root.agent.mcp_servers);

    // Review content is frozen into the input.
    let input = &created.input;
    assert!(
        input.contains("https://github.com/acme/widgets/pull/17"),
        "{input}"
    );
    assert!(input.contains("Please split this function up."), "{input}");
    assert!(input.contains("src/lib.rs:42"), "{input}");
    assert!(input.contains("rename this to `reduce`"), "{input}");
    assert!(input.contains("reply from @bob"), "{input}");
    assert!(input.contains("- clippy"), "{input}");
    assert!(
        !input.contains("- test\n"),
        "passing checks are not listed: {input}"
    );
    assert!(
        !input.contains("- docs\n"),
        "pending checks are not listed: {input}"
    );
    assert!(input.contains("Keep the public API stable."), "{input}");

    // Persisted as a normal pending session.
    let stored = state
        .sessions
        .get(&WorkspaceId::DEFAULT, &created.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.input, created.input);
    assert_eq!(stored.parent_session_id.as_deref(), Some(root.id.as_str()));
    assert!(stored.pr_status.is_none(), "tracking stays on the root");
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn agent_override_replaces_inherited_config(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let root = pr_session(&state, 17).await;
    mount_feedback(&server, 17).await;

    let (status, body) = follow_up(
        &state,
        &root.id,
        serde_json::json!({
            "agent": { "harness": "echo", "model": "", "instructions": "", "mcp_servers": [] }
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["agent"]["harness"], "echo");
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn follow_ups_chain_and_resolve_tracking_to_the_root(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let root = pr_session(&state, 17).await;
    mount_feedback(&server, 17).await;

    let (status, first) = follow_up(&state, &root.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let first_id = first["id"].as_str().unwrap().to_string();

    // Follow-up of a follow-up: no PR outcome of its own, but the chain
    // resolves to the root's PR.
    let (status, second) = follow_up(&state, &first_id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["parent_session_id"], first_id.as_str());
    assert_eq!(second["environment"]["base_branch"], "vise/feature");

    let resolved = state
        .sessions
        .resolve_tracking_root(&WorkspaceId::DEFAULT, second["id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.id, root.id);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn rejects_merged_and_closed_prs(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool.clone(), &server.uri(), pat_auth());

    // Merged per the snapshot: rejected without touching GitHub.
    let merged = pr_session(&state, 1).await;
    set_snapshot(&pool, &merged.id, PrState::Merged).await;
    let (status, _) = follow_up(&state, &merged.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let closed = pr_session(&state, 2).await;
    set_snapshot(&pool, &closed.id, PrState::Closed).await;
    let (status, _) = follow_up(&state, &closed.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Snapshot not yet synced but GitHub says closed: also rejected.
    let stale = pr_session(&state, 3).await;
    mount_pull(&server, 3, pull_json(false, false, "vise/feature", "sha1")).await;
    let (status, _) = follow_up(&state, &stale.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let approved = pr_session(&state, 4).await;
    set_snapshot(&pool, &approved.id, PrState::Approved).await;
    mount_feedback(&server, 4).await;
    let (status, _) = follow_up(&state, &approved.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn rejects_sessions_without_a_pr(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());

    let no_pr = finished_session(
        &state,
        SessionOutcome {
            kind: "pushed_no_pr".into(),
            pr_url: None,
            branch: Some("vise/x".into()),
        },
        0,
    )
    .await;
    let (status, _) = follow_up(&state, &no_pr.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _) = follow_up(&state, "ses_missing", serde_json::json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn requires_the_github_credential_provider(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), None);
    let root = pr_session(&state, 17).await;
    let (status, _) = follow_up(&state, &root.id, serde_json::json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}
