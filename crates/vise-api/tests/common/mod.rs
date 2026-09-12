//! Shared fixtures for integration tests that need a real Postgres.
//!
//! Tests call [`test_db`]; when `DATABASE_URL` is unset they print a skip
//! notice and return, so `cargo test` stays green on machines without a
//! database. Each test gets its own freshly migrated database.

#![allow(dead_code)]

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use vise_api::credentials::{CredentialProvider, IssueError, IssuedCredential};
use vise_core::sessions::model::NewSessionEvent;
use vise_core::sessions::model::{Agent, Environment, Session, SessionOutcome, SessionStatus};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::repository::SessionRepository;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
    admin_url: String,
    name: String,
}

pub async fn test_db() -> Option<TestDb> {
    let Ok(base_url) = std::env::var("DATABASE_URL") else {
        eprintln!("skipping: DATABASE_URL not set");
        return None;
    };

    let (server_url, _) = base_url
        .rsplit_once('/')
        .expect("DATABASE_URL should end in /<database>");
    let admin_url = format!("{server_url}/postgres");
    let name = format!("vise_test_{}", vise_core::id::new_id("t"));

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&admin_url)
        .await
        .expect("connect to admin database");
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .expect("create test database");
    admin.close().await;

    let url = format!("{server_url}/{name}");
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .expect("connect to test database");
    sqlx::migrate!("../vise-core/migrations")
        .run(&pool)
        .await
        .expect("run migrations");

    Some(TestDb {
        pool,
        url,
        admin_url,
        name,
    })
}

impl TestDb {
    pub fn repository(&self) -> PostgresSessionRepository {
        PostgresSessionRepository::new(self.pool.clone())
    }

    /// Drop the database. Best-effort: a leftover `vise_test_*` database is
    /// only noise, never a wrong result.
    pub async fn cleanup(self) {
        self.pool.close().await;
        if let Ok(admin) = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.admin_url)
            .await
        {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                self.name
            )))
            .execute(&admin)
            .await;
        }
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

/// Run a session through claim → events → finish so it looks exactly like one
/// a host completed, with `outcome`.
pub async fn finished_session(
    repo: &PostgresSessionRepository,
    environment: Environment,
    outcome: Option<SessionOutcome>,
    agent_events: usize,
) -> Session {
    let host = "host_test";
    let now = chrono::Utc::now();
    let session = Session {
        id: vise_core::id::new_id("ses"),
        agent: agent(),
        environment,
        input: "do the thing".into(),
        status: SessionStatus::Pending,
        host_id: None,
        lease_expires_at: None,
        started_at: None,
        finished_at: None,
        stop_reason: None,
        error: None,
        outcome: None,
        cancel_requested: false,
        parent_session_id: None,
        pr_status: None,
        created_at: now,
        updated_at: now,
    };
    let session = repo.create(session).await.unwrap();
    let claimed = repo.claim_pending(host).await.unwrap().expect("claimed");
    assert_eq!(claimed.id, session.id);

    let events: Vec<NewSessionEvent> = (1..=agent_events as i64)
        .map(|seq| NewSessionEvent {
            seq,
            payload: serde_json::json!({"sessionUpdate": "agent_message_chunk", "seq": seq}),
        })
        .collect();
    if !events.is_empty() {
        repo.append_events(host, &session.id, &events)
            .await
            .unwrap()
            .expect("held");
    }

    repo.finish(
        host,
        &session.id,
        SessionStatus::Completed,
        Some("end_turn".into()),
        None,
        outcome,
    )
    .await
    .unwrap()
    .expect("finished")
}

pub fn pr_opened(url: &str, branch: &str) -> SessionOutcome {
    SessionOutcome {
        kind: "pr_opened".into(),
        pr_url: Some(url.into()),
        branch: Some(branch.into()),
    }
}

/// Credential provider that hands out a fixed token; records nothing.
pub struct StaticCredentials(pub &'static str);

#[async_trait]
impl CredentialProvider for StaticCredentials {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError> {
        if session.environment.kind != "github_repo" {
            return Err(IssueError::NotApplicable(
                "not a github_repo session".into(),
            ));
        }
        Ok(IssuedCredential {
            secret: self.0.to_string(),
            expires_at: None,
        })
    }
}

pub fn static_credentials() -> Arc<dyn CredentialProvider> {
    Arc::new(StaticCredentials("ghs_test"))
}

// GitHub mock helpers

pub fn pr_json(
    number: u64,
    state: &str,
    merged: bool,
    head_ref: &str,
    head_sha: &str,
) -> serde_json::Value {
    serde_json::json!({
        "number": number,
        "state": state,
        "merged": merged,
        "title": "Add widgets",
        "html_url": format!("https://github.com/acme/widgets/pull/{number}"),
        "head": {"ref": head_ref, "sha": head_sha},
        "base": {"ref": "main", "sha": "base000"},
        "user": {"login": "vise[bot]"}
    })
}

pub fn review_json(
    id: u64,
    login: &str,
    state: &str,
    commit: &str,
    body: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "user": {"login": login},
        "state": state,
        "body": body,
        "commit_id": commit,
        "submitted_at": format!("2026-09-11T12:{:02}:00Z", id % 60),
    })
}

pub fn check_run_json(name: &str, status: &str, conclusion: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "status": status,
        "conclusion": conclusion,
        "details_url": format!("https://ci.example/{name}"),
    })
}

pub fn review_comment_json(
    id: u64,
    reply_to: Option<u64>,
    login: &str,
    file: &str,
    line: Option<u64>,
    body: &str,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "in_reply_to_id": reply_to,
        "user": {"login": login},
        "path": file,
        "line": line,
        "original_line": line.unwrap_or(1),
        "body": body,
        "diff_hunk": "@@ -1,2 +1,2 @@\n-old\n+new",
    })
}

pub struct GitHubMocks<'a> {
    pub server: &'a MockServer,
    pub repo: &'a str,
    pub number: u64,
}

impl GitHubMocks<'_> {
    pub async fn pull_request(&self, body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{}/pulls/{}", self.repo, self.number)))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(self.server)
            .await;
    }

    pub async fn pull_request_status(&self, status: u16) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/{}/pulls/{}", self.repo, self.number)))
            .respond_with(ResponseTemplate::new(status))
            .mount(self.server)
            .await;
    }

    pub async fn reviews(&self, body: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{}/pulls/{}/reviews",
                self.repo, self.number
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(self.server)
            .await;
    }

    pub async fn review_comments(&self, body: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{}/pulls/{}/comments",
                self.repo, self.number
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(self.server)
            .await;
    }

    pub async fn check_runs(&self, sha: &str, runs: Vec<serde_json::Value>) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/repos/{}/commits/{sha}/check-runs",
                self.repo
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total_count": runs.len(),
                "check_runs": runs,
            })))
            .mount(self.server)
            .await;
    }

    /// Open PR at `head_sha` with the given reviews and check runs.
    pub async fn open_pr(
        &self,
        head_sha: &str,
        reviews: Vec<serde_json::Value>,
        runs: Vec<serde_json::Value>,
    ) {
        self.pull_request(pr_json(
            self.number,
            "open",
            false,
            "vise/widgets",
            head_sha,
        ))
        .await;
        self.reviews(reviews).await;
        self.check_runs(head_sha, runs).await;
    }
}

pub async fn pr_events(repo: &PostgresSessionRepository, id: &str) -> Vec<serde_json::Value> {
    repo.get_events(id, 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|event| event.payload)
        .filter(|payload| {
            matches!(
                payload["type"].as_str(),
                Some("pr_state_changed" | "checks_state_changed")
            )
        })
        .collect()
}
