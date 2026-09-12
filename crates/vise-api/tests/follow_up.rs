mod common;

use std::collections::HashMap;
use std::sync::Arc;

use common::*;
use vise_api::AppState;
use vise_api::github::GitHubReadClient;
use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::model::{Agent, Session, SessionOutcome};
use vise_core::sessions::repository::SessionRepository;
use vise_core::sessions::service::SessionService;
use wiremock::MockServer;

const REPO: &str = "acme/widgets";
const PR_URL: &str = "https://github.com/acme/widgets/pull/7";

struct Api {
    base_url: String,
    http: reqwest::Client,
}

impl Api {
    async fn follow_up(&self, id: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
        let response = self
            .http
            .post(format!("{}/sessions/{id}/follow-up", self.base_url))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response
            .json::<serde_json::Value>()
            .await
            .unwrap_or(serde_json::Value::Null);
        (status, body)
    }
}

async fn serve(db: &TestDb, github: &MockServer, with_credentials: bool) -> Api {
    let mut credentials: HashMap<String, Arc<dyn vise_api::credentials::CredentialProvider>> =
        HashMap::new();
    if with_credentials {
        credentials.insert("github".into(), static_credentials());
    }
    let state = AppState {
        sessions: Arc::new(SessionService::new(db.repository())),
        hosts: Arc::new(HostService::new(PostgresHostRepository::new(
            db.pool.clone(),
        ))),
        credentials,
        github: GitHubReadClient::new(github.uri()).unwrap(),
        pr_tracking_enabled: with_credentials,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, vise_api::app(state)).await.unwrap();
    });
    Api {
        base_url: format!("http://{addr}"),
        http: reqwest::Client::new(),
    }
}

fn mocks(server: &MockServer) -> GitHubMocks<'_> {
    GitHubMocks {
        server,
        repo: REPO,
        number: 7,
    }
}

async fn mock_open_pr_with_feedback(server: &MockServer) {
    let m = mocks(server);
    m.pull_request(pr_json(7, "open", false, "vise/widgets", "sha1"))
        .await;
    m.reviews(vec![review_json(
        1,
        "bo",
        "CHANGES_REQUESTED",
        "sha1",
        "Needs tests before merge",
    )])
    .await;
    m.review_comments(vec![
        review_comment_json(
            10,
            None,
            "ana",
            "src/lib.rs",
            Some(42),
            "Rename `foo` to `widget_count`",
        ),
        review_comment_json(11, Some(10), "vise[bot]", "src/lib.rs", Some(42), "Will do"),
        review_comment_json(12, None, "bo", "README.md", None, "Document the flag"),
    ])
    .await;
    m.check_runs(
        "sha1",
        vec![
            check_run_json("clippy", "completed", Some("failure")),
            check_run_json("test", "completed", Some("success")),
            check_run_json("deploy-preview", "in_progress", None),
        ],
    )
    .await;
}

#[tokio::test]
async fn composes_input_inherits_agent_and_targets_head_branch() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    mock_open_pr_with_feedback(&github).await;
    let api = serve(&db, &github, true).await;

    let parent = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;

    let (status, body) = api
        .follow_up(
            &parent.id,
            serde_json::json!({"instructions": "Prefer small commits."}),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let follow_up: Session = serde_json::from_value(body).unwrap();

    assert_eq!(
        follow_up.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert!(matches!(
        follow_up.status,
        vise_core::sessions::model::SessionStatus::Pending
    ));
    assert!(
        follow_up.pr_status.is_none(),
        "tracking stays with the root"
    );

    // Agent config inherited verbatim.
    assert_eq!(follow_up.agent.harness, parent.agent.harness);
    assert_eq!(follow_up.agent.model, parent.agent.model);
    assert_eq!(follow_up.agent.instructions, parent.agent.instructions);
    assert_eq!(follow_up.agent.mcp_servers, parent.agent.mcp_servers);

    // Same repo, PR head branch as base.
    assert_eq!(follow_up.environment.kind, "github_repo");
    assert_eq!(follow_up.environment.repo.as_deref(), Some(REPO));
    assert_eq!(
        follow_up.environment.base_branch.as_deref(),
        Some("vise/widgets")
    );

    // Review feedback frozen into the input.
    let input = &follow_up.input;
    assert!(input.contains(PR_URL), "{input}");
    assert!(input.contains("Prefer small commits."), "{input}");
    assert!(input.contains("@bo (changes requested)"), "{input}");
    assert!(input.contains("Needs tests before merge"), "{input}");
    assert!(input.contains("`src/lib.rs` line 42 — @ana"), "{input}");
    assert!(input.contains("Rename `foo` to `widget_count`"), "{input}");
    assert!(
        input.contains("↳ @vise[bot] replied:\n> Will do"),
        "{input}"
    );
    assert!(
        input.contains("`README.md` (outdated; originally line 1)"),
        "{input}"
    );
    assert!(
        input.contains("- clippy (failure) — https://ci.example/clippy"),
        "{input}"
    );
    assert!(
        !input.contains("- test ("),
        "passing checks are not listed: {input}"
    );
    assert!(
        !input.contains("deploy-preview"),
        "pending checks are not listed: {input}"
    );

    // Persisted, not just returned.
    let stored = repo.get(&follow_up.id).await.unwrap().unwrap();
    assert_eq!(
        stored.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );
    assert_eq!(stored.input, follow_up.input);

    // The token minted by the server credential was used against GitHub.
    let requests = github.received_requests().await.unwrap();
    assert!(!requests.is_empty());
    for request in requests {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            "Bearer ghs_test"
        );
    }

    db.cleanup().await;
}

#[tokio::test]
async fn agent_override_replaces_inherited_config() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    mock_open_pr_with_feedback(&github).await;
    let api = serve(&db, &github, true).await;

    let parent = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let override_agent = Agent {
        harness: "echo".into(),
        model: "m".into(),
        instructions: "".into(),
        mcp_servers: vec![],
    };

    let (status, body) = api
        .follow_up(&parent.id, serde_json::json!({"agent": override_agent}))
        .await;
    assert_eq!(status, 201, "{body}");
    let follow_up: Session = serde_json::from_value(body).unwrap();
    assert_eq!(follow_up.agent.harness, "echo");
    assert!(!follow_up.input.contains("No open review comments"));

    db.cleanup().await;
}

#[tokio::test]
async fn follow_ups_chain_and_resolve_to_the_root_pr() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    mock_open_pr_with_feedback(&github).await;
    let api = serve(&db, &github, true).await;

    let root = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let (status, body) = api.follow_up(&root.id, serde_json::json!({})).await;
    assert_eq!(status, 201, "{body}");
    let first: Session = serde_json::from_value(body).unwrap();

    // Simulate the first follow-up finishing with pr_updated (which is never
    // itself tracked), then request a follow-up on *it*.
    let host = "host_b";
    let claimed = repo.claim_pending(host).await.unwrap().unwrap();
    assert_eq!(claimed.id, first.id);
    repo.finish(
        host,
        &first.id,
        vise_core::sessions::model::SessionStatus::Completed,
        None,
        None,
        Some(SessionOutcome {
            kind: "pr_updated".into(),
            pr_url: Some(PR_URL.into()),
            branch: Some("vise/widgets".into()),
        }),
    )
    .await
    .unwrap()
    .unwrap();

    let (status, body) = api.follow_up(&first.id, serde_json::json!({})).await;
    assert_eq!(status, 201, "{body}");
    let second: Session = serde_json::from_value(body).unwrap();
    assert_eq!(second.parent_session_id.as_deref(), Some(first.id.as_str()));
    assert_eq!(
        second.environment.base_branch.as_deref(),
        Some("vise/widgets")
    );
    assert!(second.input.contains(PR_URL));

    db.cleanup().await;
}

#[tokio::test]
async fn rejects_when_snapshot_says_merged_or_closed() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    mock_open_pr_with_feedback(&github).await;
    let api = serve(&db, &github, true).await;

    for state in ["merged", "closed"] {
        let parent = finished_session(
            &repo,
            github_env(REPO),
            Some(pr_opened(PR_URL, "vise/widgets")),
            0,
        )
        .await;
        sqlx::query("UPDATE sessions SET pr_status = $2 WHERE id = $1")
            .bind(&parent.id)
            .bind(serde_json::json!({"state": state, "last_synced_at": "2026-09-11T12:00:00Z"}))
            .execute(&db.pool)
            .await
            .unwrap();

        let (status, _) = api.follow_up(&parent.id, serde_json::json!({})).await;
        assert_eq!(status, 409, "snapshot {state} must reject");
    }
    assert!(
        github.received_requests().await.unwrap().is_empty(),
        "rejected before touching GitHub"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn rejects_when_github_says_the_pr_is_no_longer_open() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    let m = mocks(&github);
    m.pull_request(pr_json(7, "closed", true, "vise/widgets", "sha1"))
        .await;
    m.reviews(vec![]).await;
    m.review_comments(vec![]).await;
    m.check_runs("sha1", vec![]).await;
    let api = serve(&db, &github, true).await;

    // Snapshot still says review_pending (poller lag); GitHub wins.
    let parent = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let (status, _) = api.follow_up(&parent.id, serde_json::json!({})).await;
    assert_eq!(status, 409);

    db.cleanup().await;
}

#[tokio::test]
async fn rejects_sessions_that_did_not_open_a_pr() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    let api = serve(&db, &github, true).await;

    let no_pr = finished_session(
        &repo,
        github_env(REPO),
        Some(SessionOutcome {
            kind: "pushed_no_pr".into(),
            pr_url: None,
            branch: Some("x".into()),
        }),
        0,
    )
    .await;
    let (status, _) = api.follow_up(&no_pr.id, serde_json::json!({})).await;
    assert_eq!(status, 422);

    let (status, _) = api.follow_up("ses_missing", serde_json::json!({})).await;
    assert_eq!(status, 404);

    db.cleanup().await;
}

#[tokio::test]
async fn requires_a_configured_github_credential() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    let api = serve(&db, &github, false).await;

    let parent = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let (status, _) = api.follow_up(&parent.id, serde_json::json!({})).await;
    assert_eq!(status, 503);

    db.cleanup().await;
}

#[tokio::test]
async fn github_failures_surface_as_bad_gateway() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let github = MockServer::start().await;
    mocks(&github).pull_request_status(500).await;
    let api = serve(&db, &github, true).await;

    let parent = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let (status, _) = api.follow_up(&parent.id, serde_json::json!({})).await;
    assert_eq!(status, 502);

    db.cleanup().await;
}
