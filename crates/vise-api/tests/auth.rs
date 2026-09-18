//! Caller identity on user-facing routes: open by default, a static bearer
//! token when configured, and swappable for an external extractor.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header, request::Parts};
use common::*;
use sqlx::PgPool;
use tower::ServiceExt;
use vise_api::AppState;
use vise_api::auth::{Caller, CallerError, CallerExtractor, OpenAccess, StaticToken};
use vise_core::workspaces::model::WorkspaceId;

const API_TOKEN: &str = "s3cret-api-token";

fn open_state(pool: PgPool) -> AppState {
    app_state(pool, "http://github.invalid", None)
}

fn token_state(pool: PgPool) -> AppState {
    let mut state = open_state(pool);
    state.caller = Arc::new(StaticToken::new(API_TOKEN));
    state
}

struct Call {
    method: &'static str,
    path: String,
    body: Option<serde_json::Value>,
    bearer: Option<String>,
}

fn call(method: &'static str, path: impl Into<String>) -> Call {
    Call {
        method,
        path: path.into(),
        body: None,
        bearer: None,
    }
}

impl Call {
    fn json(mut self, body: serde_json::Value) -> Self {
        self.body = Some(body);
        self
    }

    fn bearer(mut self, token: &str) -> Self {
        self.bearer = Some(token.to_string());
        self
    }

    async fn send(
        self,
        state: &AppState,
    ) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
        let mut request = Request::builder().method(self.method).uri(self.path);
        if let Some(token) = self.bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match self.body {
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
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, headers, json)
    }
}

fn create_session_body() -> serde_json::Value {
    serde_json::json!({
        "agent": agent(),
        "environment": { "kind": "self_hosted" },
        "input": "hello"
    })
}

fn enroll_body(name: &str) -> serde_json::Value {
    serde_json::json!({ "name": name })
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn open_access_leaves_user_facing_routes_unauthenticated(pool: PgPool) {
    let state = open_state(pool);

    let (status, _, body) = call("POST", "/hosts")
        .json(enroll_body("laptop"))
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, _, body) = call("POST", "/sessions")
        .json(create_session_body())
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    for (method, path) in [
        ("GET", "/sessions".to_string()),
        ("GET", format!("/sessions/{id}")),
        ("GET", format!("/sessions/{id}/events")),
        ("GET", "/hosts".to_string()),
    ] {
        let (status, _, body) = call(method, path.clone()).send(&state).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    }

    // A stray bearer token is ignored rather than rejected.
    let (status, _, _) = call("GET", "/sessions")
        .bearer("whatever")
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn static_token_rejects_requests_without_it(pool: PgPool) {
    let state = token_state(pool);

    let attempts: Vec<(&str, String, Option<serde_json::Value>)> = vec![
        ("GET", "/sessions".into(), None),
        ("POST", "/sessions".into(), Some(create_session_body())),
        ("GET", "/sessions/nope".into(), None),
        ("GET", "/sessions/nope/events".into(), None),
        ("GET", "/sessions/nope/events/stream".into(), None),
        ("POST", "/sessions/nope/cancel".into(), None),
        (
            "POST",
            "/sessions/nope/follow-up".into(),
            Some(serde_json::json!({})),
        ),
        ("POST", "/hosts".into(), Some(enroll_body("laptop"))),
        ("GET", "/hosts".into(), None),
    ];

    for (method, path, body) in attempts {
        for bearer in [None, Some("wrong-token"), Some("")] {
            let mut request = call(method, path.clone());
            if let Some(body) = body.clone() {
                request = request.json(body);
            }
            if let Some(bearer) = bearer {
                request = request.bearer(bearer);
            }
            let (status, headers, _) = request.send(&state).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{method} {path} with bearer {bearer:?}"
            );
            assert_eq!(
                headers
                    .get(header::WWW_AUTHENTICATE)
                    .map(|v| v.to_str().unwrap()),
                Some("Bearer"),
                "{method} {path} advertises the bearer scheme"
            );
        }
    }

    // Nothing was created behind the 401s.
    let (status, _, body) = call("GET", "/hosts").bearer(API_TOKEN).send(&state).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().map(Vec::len), Some(0));
    let (status, _, body) = call("GET", "/sessions")
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["sessions"].as_array().map(Vec::len), Some(0));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn static_token_accepts_requests_carrying_it(pool: PgPool) {
    let state = token_state(pool);

    let (status, _, body) = call("POST", "/hosts")
        .json(enroll_body("laptop"))
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["token"].as_str().unwrap().starts_with("vhost_"));

    let (status, _, body) = call("POST", "/sessions")
        .json(create_session_body())
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();

    for (method, path) in [
        ("GET", "/sessions".to_string()),
        ("GET", format!("/sessions/{id}")),
        ("GET", format!("/sessions/{id}/events")),
        ("GET", "/hosts".to_string()),
        ("POST", format!("/sessions/{id}/cancel")),
    ] {
        let (status, _, body) = call(method, path.clone())
            .bearer(API_TOKEN)
            .send(&state)
            .await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    }
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn host_protocol_routes_keep_their_own_host_auth(pool: PgPool) {
    let state = token_state(pool);

    let (status, _, body) = call("POST", "/hosts")
        .json(enroll_body("laptop"))
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let host_token = body["token"].as_str().unwrap().to_string();

    // The host token alone is enough for the pull protocol...
    let (status, _, body) = call("POST", "/hosts/claim")
        .json(serde_json::json!({}))
        .bearer(&host_token)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["session"].is_null());

    // ...and the API token is not: it identifies a user, not a host.
    let (status, _, _) = call("POST", "/hosts/claim")
        .json(serde_json::json!({}))
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = call("POST", "/hosts/sessions/nope/heartbeat")
        .bearer(API_TOKEN)
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// An extractor an external composition might supply: identifies the caller
/// from an `x-user` header and puts them in a workspace of their own.
struct HeaderUser;

#[async_trait]
impl CallerExtractor for HeaderUser {
    async fn extract(&self, parts: &mut Parts) -> Result<Caller, CallerError> {
        let user = parts
            .headers
            .get("x-user")
            .and_then(|value| value.to_str().ok())
            .ok_or(CallerError::Unauthorized)?;
        Ok(Caller::new(user).with_workspace(format!("ws-{user}")))
    }
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn an_external_extractor_replaces_the_oss_ones(pool: PgPool) {
    // A session created through the OSS extractor lands in the default
    // workspace.
    let mut state = open_state(pool);
    let (status, _, body) = call("POST", "/sessions")
        .json(create_session_body())
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["workspace_id"], WorkspaceId::DEFAULT.as_str());

    state.caller = Arc::new(HeaderUser);

    let (status, _, _) = call("GET", "/sessions").send(&state).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Alice is scoped to her own workspace, so the default workspace's
    // session is invisible to her: the routes read the workspace off the
    // caller, not off the state.
    let as_alice = |path: String| {
        vise_api::app(state.clone()).oneshot(
            Request::get(path)
                .header("x-user", "alice")
                .body(Body::empty())
                .unwrap(),
        )
    };
    let response = as_alice("/sessions".into()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(listed["sessions"].as_array().map(Vec::len), Some(0));

    let response = as_alice(format!("/sessions/{id}")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Host-protocol routes are untouched by the swap.
    let (status, _, _) = call("POST", "/hosts/claim")
        .json(serde_json::json!({}))
        .send(&state)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And the state's own default is still OpenAccess-compatible.
    state.caller = Arc::new(OpenAccess);
    let (status, _, _) = call("GET", "/sessions").send(&state).await;
    assert_eq!(status, StatusCode::OK);
}

#[test]
fn a_caller_defaults_to_the_default_workspace() {
    let caller = Caller::new("alice").with_workspace("ws-1");
    assert_eq!(caller.subject.as_deref(), Some("alice"));
    assert_eq!(caller.workspace, WorkspaceId::new("ws-1"));
    assert_eq!(Caller::new("bob").workspace, WorkspaceId::DEFAULT);
    assert_eq!(Caller::anonymous().workspace, WorkspaceId::DEFAULT);
}
