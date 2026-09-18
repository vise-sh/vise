//! Host claim protocol: the claim response carries the workspace's
//! event-fidelity policy so the host can redact events before upload.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use common::*;
use sqlx::PgPool;
use tower::ServiceExt;

async fn claim_over_http(
    state: &vise_api::AppState,
    token: &str,
) -> (StatusCode, serde_json::Value) {
    let response = vise_api::app(state.clone())
        .oneshot(
            Request::post("/hosts/claim")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::from(
                    serde_json::json!({ "harnesses": [], "environment_types": [] }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn enrolled_host_token(state: &vise_api::AppState) -> String {
    state
        .hosts
        .enroll(state.workspace.clone(), "fidelity-test-host".into())
        .await
        .unwrap()
        .token
}

async fn queue_session(state: &vise_api::AppState) -> String {
    state
        .sessions
        .create(
            state.workspace.clone(),
            agent(),
            github_env("acme/widgets"),
            "do the thing".into(),
            None,
        )
        .await
        .unwrap()
        .id
}

async fn set_workspace_settings(pool: &PgPool, settings: serde_json::Value) {
    sqlx::query("UPDATE workspaces SET settings = $1 WHERE id = 'default'")
        .bind(settings)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn claim_defaults_to_full_fidelity(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);
    let token = enrolled_host_token(&state).await;
    let session_id = queue_session(&state).await;

    let (status, body) = claim_over_http(&state, &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["session"]["id"], serde_json::json!(session_id));
    assert_eq!(body["event_fidelity"], serde_json::json!("full"));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn claim_carries_the_workspaces_redacted_policy(pool: PgPool) {
    set_workspace_settings(&pool, serde_json::json!({ "event_fidelity": "redacted" })).await;

    let state = app_state(pool, "http://unused", None);
    let token = enrolled_host_token(&state).await;
    let session_id = queue_session(&state).await;

    let (status, body) = claim_over_http(&state, &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["session"]["id"], serde_json::json!(session_id));
    assert_eq!(body["event_fidelity"], serde_json::json!("redacted"));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn claim_fails_closed_on_an_unrecognized_policy(pool: PgPool) {
    // A typo'd policy must not silently resolve to full content.
    set_workspace_settings(&pool, serde_json::json!({ "event_fidelity": "partial" })).await;

    let state = app_state(pool, "http://unused", None);
    let token = enrolled_host_token(&state).await;

    let (status, body) = claim_over_http(&state, &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["event_fidelity"], serde_json::json!("redacted"));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn empty_claim_still_reports_the_policy(pool: PgPool) {
    set_workspace_settings(&pool, serde_json::json!({ "event_fidelity": "redacted" })).await;

    let state = app_state(pool, "http://unused", None);
    let token = enrolled_host_token(&state).await;

    let (status, body) = claim_over_http(&state, &token).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["session"], serde_json::Value::Null);
    assert_eq!(body["event_fidelity"], serde_json::json!("redacted"));
}
