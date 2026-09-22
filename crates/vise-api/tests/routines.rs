//! Routine HTTP API: CRUD, off-schedule runs, and run listing, all scoped to
//! the caller's workspace and driven through the real `app` router.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

/// A valid routine body: a github_repo spec on a daily 02:00 schedule.
fn valid_body() -> Value {
    json!({
        "name": "nightly triage",
        "cron": "0 2 * * *",
        "timezone": "America/New_York",
        "spec": {
            "agent": {
                "harness": "claude-code",
                "model": "claude-fable-5-1",
                "instructions": "triage the backlog",
                "mcp_servers": ["linear"]
            },
            "environment": {
                "kind": "github_repo",
                "repo": "acme/widgets",
                "base_branch": "main"
            },
            "input": "triage"
        }
    })
}

async fn send(
    state: &vise_api::AppState,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    let request = match body {
        Some(body) => builder
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => {
            builder = builder.header("content-type", "application/json");
            builder.body(Body::empty()).unwrap()
        }
    };

    let response = vise_api::app(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn create_returns_routine_with_next_run_at(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    let (status, body) = send(&state, "POST", "/routines", Some(valid_body())).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["name"], json!("nightly triage"));
    assert_eq!(body["enabled"], json!(true));
    let next_run_at = body["next_run_at"].as_str().expect("next_run_at is a string");
    let next: chrono::DateTime<chrono::Utc> = next_run_at.parse().expect("next_run_at parses");
    assert!(next > chrono::Utc::now(), "next_run_at must be in the future");
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn create_rejects_bad_cron(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    // Sub-15-minute floor.
    let mut sub_floor = valid_body();
    sub_floor["cron"] = json!("*/5 * * * *");
    let (status, _) = send(&state, "POST", "/routines", Some(sub_floor)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Malformed cron.
    let mut malformed = valid_body();
    malformed["cron"] = json!("not a cron");
    let (status, _) = send(&state, "POST", "/routines", Some(malformed)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn list_and_get_are_workspace_scoped(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    let (_, created) = send(&state, "POST", "/routines", Some(valid_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, list) = send(&state, "GET", "/routines", None).await;
    assert_eq!(status, StatusCode::OK);
    let routines = list["routines"].as_array().unwrap();
    assert_eq!(routines.len(), 1);
    assert_eq!(routines[0]["id"], json!(id));

    let (status, got) = send(&state, "GET", &format!("/routines/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["id"], json!(id));

    let (status, _) = send(&state, "GET", "/routines/rtn_missing", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn patch_disables_then_reenables(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    let (_, created) = send(&state, "POST", "/routines", Some(valid_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, patched) = send(
        &state,
        "PATCH",
        &format!("/routines/{id}"),
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["enabled"], json!(false));

    let (_, got) = send(&state, "GET", &format!("/routines/{id}"), None).await;
    assert_eq!(got["enabled"], json!(false));

    let (status, patched) = send(
        &state,
        "PATCH",
        &format!("/routines/{id}"),
        Some(json!({ "enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["enabled"], json!(true));

    let (_, got) = send(&state, "GET", &format!("/routines/{id}"), None).await;
    assert_eq!(got["enabled"], json!(true));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn run_now_returns_session_and_appears_in_runs(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    let (_, created) = send(&state, "POST", "/routines", Some(valid_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, session) = send(&state, "POST", &format!("/routines/{id}/run"), None).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(session["routine_id"], json!(id));
    let session_id = session["id"].as_str().unwrap().to_string();

    let (status, runs) = send(&state, "GET", &format!("/routines/{id}/runs"), None).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = runs["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], json!(session_id));

    // A run against an unknown routine is a 404, not an empty 200.
    let (status, _) = send(&state, "POST", "/routines/rtn_missing/run", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(&state, "GET", "/routines/rtn_missing/runs", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn delete_then_get_404(pool: PgPool) {
    let state = app_state(pool, "http://unused", None);

    let (_, created) = send(&state, "POST", "/routines", Some(valid_body())).await;
    let id = created["id"].as_str().unwrap().to_string();

    let (status, _) = send(&state, "DELETE", &format!("/routines/{id}"), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _) = send(&state, "GET", &format!("/routines/{id}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
