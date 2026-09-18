#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::PgPool;
use vise_api::AppState;
use vise_api::credentials::CredentialProvider;
use vise_api::github::{GitHubApi, GithubAuth};
use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::model::{
    Agent, Environment, NewSessionEvent, Session, SessionOutcome, SessionStatus,
};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};
use vise_core::workspaces::model::WorkspaceId;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const TOKEN: &str = "github_pat_static_test_token";

/// PAT-mode GitHub auth with one fixed token, the way a server configured
/// with `VISE_GITHUB_PAT` (and no App) runs.
pub fn pat_auth() -> Option<GithubAuth> {
    Some(GithubAuth::Pat(TOKEN.to_string()))
}

/// Application state wired like `vise-server` does it: `github` (if any)
/// supplies both the "github" credential provider and the read client
/// pointed at `github_base`.
pub fn app_state(pool: PgPool, github_base: &str, github: Option<GithubAuth>) -> AppState {
    let mut credentials: HashMap<String, Arc<dyn CredentialProvider>> = HashMap::new();
    let github = github.map(|auth| {
        credentials.insert("github".to_string(), auth.credential_provider());
        GitHubApi::new(github_base.to_string(), auth)
    });
    AppState {
        sessions: Arc::new(SessionService::new(PostgresSessionRepository::new(
            pool.clone(),
        ))),
        hosts: Arc::new(HostService::new(PostgresHostRepository::new(pool))),
        workspace: WorkspaceId::DEFAULT,
        credentials,
        github,
    }
}

pub fn agent() -> Agent {
    Agent {
        harness: "claude-code".into(),
        model: "claude-fable-5-1".into(),
        instructions: "be terse".into(),
        mcp_servers: vec!["linear".into()],
    }
}

pub fn github_env(repo: &str) -> Environment {
    Environment {
        kind: "github_repo".into(),
        repo: Some(repo.into()),
        base_branch: Some("main".into()),
    }
}

/// Enroll a host in the state's workspace. Claims derive their scope from
/// the host row, so a session can only be driven through a real host.
pub async fn enrolled_host(state: &AppState) -> vise_core::hosts::model::Host {
    let name = vise_core::id::new_id("test-host");
    state
        .hosts
        .enroll(state.workspace.clone(), name)
        .await
        .unwrap()
        .host
}

/// Create a session, run it through claim → events → finish so it ends up
/// `completed` with the given outcome, like a real host would leave it.
pub async fn finished_session(
    state: &AppState,
    outcome: SessionOutcome,
    host_events: usize,
) -> Session {
    let host = enrolled_host(state).await;
    let session = state
        .sessions
        .create(
            state.workspace.clone(),
            agent(),
            github_env("acme/widgets"),
            "do the thing".into(),
            None,
        )
        .await
        .unwrap();
    let claimed = state.sessions.claim(&host.id).await.unwrap().unwrap();
    assert_eq!(claimed.id, session.id);

    let events: Vec<NewSessionEvent> = (1..=host_events as i64)
        .map(|seq| NewSessionEvent {
            seq,
            payload: serde_json::json!({ "sessionUpdate": "agent_message_chunk", "seq": seq }),
        })
        .collect();
    if !events.is_empty() {
        state
            .sessions
            .append_events(&host.id, &session.id, &events)
            .await
            .unwrap()
            .unwrap();
    }

    state
        .sessions
        .finish(
            &host.id,
            &session.id,
            SessionStatus::Completed,
            Some("end_turn".into()),
            None,
            Some(outcome),
        )
        .await
        .unwrap()
        .unwrap()
}

pub fn pr_opened(number: u64) -> SessionOutcome {
    SessionOutcome {
        kind: "pr_opened".into(),
        pr_url: Some(format!("https://github.com/acme/widgets/pull/{number}")),
        branch: Some("vise/feature".into()),
    }
}

pub async fn pr_session(state: &AppState, number: u64) -> Session {
    finished_session(state, pr_opened(number), 2).await
}

// --- GitHub mocks ----------------------------------------------------------

pub fn pull_json(open: bool, merged: bool, head_ref: &str, sha: &str) -> serde_json::Value {
    serde_json::json!({
        "state": if open { "open" } else { "closed" },
        "merged": merged,
        "html_url": "https://github.com/acme/widgets/pull/17",
        "head": { "ref": head_ref, "sha": sha }
    })
}

pub async fn mount_pull(server: &MockServer, number: u64, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/acme/widgets/pulls/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_reviews(server: &MockServer, number: u64, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/acme/widgets/pulls/{number}/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_review_comments(server: &MockServer, number: u64, body: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/acme/widgets/pulls/{number}/comments")))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

pub async fn mount_check_runs(server: &MockServer, sha: &str, runs: serde_json::Value) {
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/acme/widgets/commits/{sha}/check-runs"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": runs.as_array().map(Vec::len).unwrap_or(0),
            "check_runs": runs
        })))
        .mount(server)
        .await;
}

pub fn review(login: &str, state: &str, sha: &str, minute: u32) -> serde_json::Value {
    serde_json::json!({
        "user": { "login": login },
        "state": state,
        "body": "",
        "submitted_at": format!("2026-09-12T10:{minute:02}:00Z"),
        "commit_id": sha
    })
}

pub fn check_run(name: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
    serde_json::json!({ "name": name, "status": status, "conclusion": conclusion })
}

/// A healthy open PR: one approval on the head commit, one passing check.
pub async fn mount_open_pr(server: &MockServer, number: u64) {
    mount_pull(
        server,
        number,
        pull_json(true, false, "vise/feature", "sha1"),
    )
    .await;
    mount_reviews(
        server,
        number,
        serde_json::json!([review("alice", "APPROVED", "sha1", 1)]),
    )
    .await;
    mount_review_comments(server, number, serde_json::json!([])).await;
    mount_check_runs(
        server,
        "sha1",
        serde_json::json!([check_run("test", "completed", Some("success"))]),
    )
    .await;
}
