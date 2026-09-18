//! Enrollment tokens over HTTP: mint → exchange → the new host drives the
//! pull protocol; revocation and max_uses reject with 401; listings carry no
//! secret material.

mod common;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use common::*;
use sqlx::PgPool;
use tower::ServiceExt;
use vise_api::AppState;

fn open_state(pool: PgPool) -> AppState {
    app_state(pool, "http://github.invalid", None)
}

async fn send(
    state: &AppState,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(body) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = vise_api::app(state.clone())
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn mint(state: &AppState, max_uses: Option<i64>) -> serde_json::Value {
    let (status, body) = send(
        state,
        "POST",
        "/hosts/enrollment-tokens",
        None,
        Some(serde_json::json!({ "max_uses": max_uses })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn minted_token_enrolls_an_ephemeral_host_that_runs_an_echo_session(pool: PgPool) {
    let state = open_state(pool);

    let minted = mint(&state, Some(2)).await;
    let secret = minted["secret"].as_str().unwrap();
    assert!(secret.starts_with("venroll_"), "{secret}");
    assert_eq!(minted["token"]["uses"], 0);
    assert_eq!(minted["token"]["max_uses"], 2);

    // A booting host exchanges the secret, with no API credential at all.
    let (status, enrolled) = send(
        &state,
        "POST",
        "/hosts/exchange",
        None,
        Some(serde_json::json!({ "token": secret, "name_prefix": "ci" })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{enrolled}");
    assert_eq!(enrolled["host"]["ephemeral"], true);
    assert_eq!(enrolled["host"]["workspace_id"], "default");
    let name = enrolled["host"]["name"].as_str().unwrap();
    assert!(name.starts_with("ci-"), "{name}");
    let host_token = enrolled["token"].as_str().unwrap().to_string();
    assert!(host_token.starts_with("vhost_"), "{host_token}");

    // The fresh vhost_ token drives the pull protocol like any other host's.
    let (status, body) = send(
        &state,
        "POST",
        "/hosts/claim",
        Some(&host_token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["session"].is_null());

    // A user queues an echo session through the sessions API...
    let (status, session) = send(
        &state,
        "POST",
        "/sessions",
        None,
        Some(serde_json::json!({
            "agent": {
                "harness": "echo",
                "model": "",
                "instructions": "",
                "mcp_servers": []
            },
            "environment": { "kind": "self_hosted" },
            "input": "say hello"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{session}");
    let session_id = session["id"].as_str().unwrap().to_string();

    // ...which the enrolled host claims and runs to completion.
    let (status, body) = send(
        &state,
        "POST",
        "/hosts/claim",
        Some(&host_token),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["session"]["id"], session_id.as_str());
    assert_eq!(body["session"]["status"], "running");

    let (status, body) = send(
        &state,
        "POST",
        &format!("/hosts/sessions/{session_id}/events"),
        Some(&host_token),
        Some(serde_json::json!({
            "events": [{
                "seq": 1,
                "payload": { "sessionUpdate": "agent_message_chunk", "text": "hello" }
            }]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, finished) = send(
        &state,
        "POST",
        &format!("/hosts/sessions/{session_id}/finish"),
        Some(&host_token),
        Some(serde_json::json!({
            "status": "completed",
            "stop_reason": "end_turn",
            "error": null
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{finished}");
    assert_eq!(finished["status"], "completed");

    // The user-facing API sees the completed session and its events.
    let (status, fetched) = send(
        &state,
        "GET",
        &format!("/sessions/{session_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["status"], "completed");

    let (status, events) = send(
        &state,
        "GET",
        &format!("/sessions/{session_id}/events"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{events}");
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["payload"]["text"], "hello");

    // Listing counts the use and never echoes secret material.
    let (status, listed) = send(&state, "GET", "/hosts/enrollment-tokens", None, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["uses"], 1);
    let fields = listed[0].as_object().unwrap();
    assert!(!fields.contains_key("secret"), "{listed:?}");
    assert!(!fields.contains_key("token_hash"), "{listed:?}");
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn exchange_rejects_exhausted_unknown_and_revoked_tokens(pool: PgPool) {
    let state = open_state(pool);

    // Exhausted: a single-use token works once.
    let minted = mint(&state, Some(1)).await;
    let secret = minted["secret"].as_str().unwrap();
    let exchange = |secret: &str| {
        let state = state.clone();
        let secret = secret.to_string();
        async move {
            send(
                &state,
                "POST",
                "/hosts/exchange",
                None,
                Some(serde_json::json!({ "token": secret })),
            )
            .await
        }
    };
    let (status, _) = exchange(secret).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = exchange(secret).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "past max_uses");

    // Unknown secret.
    let (status, _) = exchange("venroll_never_minted").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Revoked: revocation is idempotent and takes effect immediately.
    let minted = mint(&state, None).await;
    let id = minted["token"]["id"].as_str().unwrap();
    let secret = minted["secret"].as_str().unwrap();
    let (status, revoked) = send(
        &state,
        "POST",
        &format!("/hosts/enrollment-tokens/{id}/revoke"),
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert!(!revoked["revoked_at"].is_null());
    let (status, _) = exchange(secret).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked token");

    // Revoking a token that does not exist is a 404, not a silent success.
    let (status, _) = send(
        &state,
        "POST",
        "/hosts/enrollment-tokens/enr_missing/revoke",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Only the one successful exchange enrolled a host.
    let (_, hosts) = send(&state, "GET", "/hosts", None, None).await;
    assert_eq!(hosts.as_array().map(Vec::len), Some(1));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn mint_rejects_a_nonpositive_use_cap(pool: PgPool) {
    let state = open_state(pool);
    let (status, _) = send(
        &state,
        "POST",
        "/hosts/enrollment-tokens",
        None,
        Some(serde_json::json!({ "max_uses": 0 })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, listed) = send(&state, "GET", "/hosts/enrollment-tokens", None, None).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(0));
}
