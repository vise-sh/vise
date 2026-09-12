mod common;

use std::time::Duration;

use common::*;
use vise_api::github::GitHubReadClient;
use vise_api::pr_tracking::{PrTracker, TickReport};
use vise_core::sessions::model::{ChecksState, PrState, SessionOutcome};
use vise_core::sessions::repository::SessionRepository;
use wiremock::MockServer;

const REPO: &str = "acme/widgets";
const PR_URL: &str = "https://github.com/acme/widgets/pull/7";

fn tracker(db: &TestDb, server: &MockServer) -> PrTracker {
    PrTracker::new(
        db.repository(),
        GitHubReadClient::new(server.uri()).unwrap(),
        static_credentials(),
        Duration::from_secs(60),
    )
}

fn mocks(server: &MockServer) -> GitHubMocks<'_> {
    GitHubMocks {
        server,
        repo: REPO,
        number: 7,
    }
}

#[tokio::test]
async fn first_sync_writes_snapshot_and_events_after_agent_events() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        3,
    )
    .await;
    mocks(&server)
        .open_pr(
            "sha1",
            vec![review_json(1, "ana", "APPROVED", "sha1", "")],
            vec![check_run_json("ci", "in_progress", None)],
        )
        .await;

    let report = tracker(&db, &server).tick().await;
    assert_eq!(
        report,
        TickReport {
            synced: 1,
            skipped: 0,
            backoff: None
        }
    );

    let synced = repo.get(&session.id).await.unwrap().unwrap();
    let status = synced.pr_status.expect("snapshot written");
    assert_eq!(status.state, PrState::Approved);
    assert_eq!(status.checks, Some(ChecksState::Pending));

    let events = repo.get_events(&session.id, 0, 100).await.unwrap();
    assert_eq!(events.len(), 5, "3 agent events + 2 transition events");
    assert_eq!(
        events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(events[3].payload["type"], "pr_state_changed");
    assert_eq!(events[3].payload["from"], serde_json::Value::Null);
    assert_eq!(events[3].payload["to"], "approved");
    assert_eq!(events[4].payload["type"], "checks_state_changed");
    assert_eq!(events[4].payload["to"], "pending");

    db.cleanup().await;
}

#[tokio::test]
async fn no_change_appends_nothing_but_touches_last_synced_at() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    mocks(&server)
        .open_pr(
            "sha1",
            vec![],
            vec![check_run_json("ci", "completed", Some("success"))],
        )
        .await;

    let tracker = tracker(&db, &server);
    tracker.tick().await;
    let first = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(first.state, PrState::ReviewPending);
    assert_eq!(first.checks, Some(ChecksState::Passing));

    tokio::time::sleep(Duration::from_millis(20)).await;
    let report = tracker.tick().await;
    assert_eq!(report.synced, 1);

    let second = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(second.state, first.state);
    assert_eq!(second.checks, first.checks);
    assert!(
        second.last_synced_at > first.last_synced_at,
        "last_synced_at must advance"
    );
    assert_eq!(
        pr_events(&repo, &session.id).await.len(),
        2,
        "no new events on no-change"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn transitions_append_one_event_per_changed_dimension() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    mocks(&server)
        .open_pr(
            "sha1",
            vec![review_json(1, "ana", "APPROVED", "sha1", "")],
            vec![check_run_json("ci", "completed", Some("success"))],
        )
        .await;
    let tracker = tracker(&db, &server);
    tracker.tick().await;

    // Reviewer asks for changes; checks unchanged.
    server.reset().await;
    mocks(&server)
        .open_pr(
            "sha1",
            vec![
                review_json(1, "ana", "APPROVED", "sha1", ""),
                review_json(2, "bo", "CHANGES_REQUESTED", "sha1", ""),
            ],
            vec![check_run_json("ci", "completed", Some("success"))],
        )
        .await;
    tracker.tick().await;

    let events = pr_events(&repo, &session.id).await;
    assert_eq!(events.len(), 3);
    assert_eq!(
        events[2],
        serde_json::json!({"type": "pr_state_changed", "from": "approved", "to": "changes_requested"})
    );

    // Force-push: approvals become stale, checks restart.
    server.reset().await;
    mocks(&server)
        .open_pr(
            "sha2",
            vec![
                review_json(1, "ana", "APPROVED", "sha1", ""),
                review_json(3, "bo", "APPROVED", "sha1", ""),
            ],
            vec![check_run_json("ci", "queued", None)],
        )
        .await;
    tracker.tick().await;

    let status = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(
        status.state,
        PrState::ReviewPending,
        "approvals on the old head are stale"
    );
    assert_eq!(status.checks, Some(ChecksState::Pending));
    let events = pr_events(&repo, &session.id).await;
    assert_eq!(events.len(), 5);
    assert_eq!(events[3]["to"], "review_pending");
    assert_eq!(
        events[4],
        serde_json::json!({"type": "checks_state_changed", "from": "passing", "to": "pending"})
    );

    db.cleanup().await;
}

#[tokio::test]
async fn merged_is_terminal_and_leaves_the_work_list() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let m = mocks(&server);
    m.pull_request(pr_json(7, "closed", true, "vise/widgets", "sha1"))
        .await;
    m.reviews(vec![]).await;
    m.check_runs("sha1", vec![]).await;

    let tracker = tracker(&db, &server);
    let report = tracker.tick().await;
    assert_eq!(report.synced, 1);

    let status = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(status.state, PrState::Merged);
    assert_eq!(status.checks, None);
    assert_eq!(
        pr_events(&repo, &session.id).await,
        vec![serde_json::json!({"type": "pr_state_changed", "from": null, "to": "merged"})]
    );

    // Terminal: the next tick must not touch GitHub or the row at all.
    server.reset().await;
    let report = tracker.tick().await;
    assert_eq!(report, TickReport::default());
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "merged PRs are never polled again"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn persistent_not_found_marks_sync_error_then_recovers() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    mocks(&server).open_pr("sha1", vec![], vec![]).await;
    let tracker = tracker(&db, &server);
    tracker.tick().await;
    assert_eq!(
        repo.get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .unwrap()
            .state,
        PrState::ReviewPending
    );

    server.reset().await;
    mocks(&server).pull_request_status(404).await;

    // Two failures: skipped, snapshot untouched, no events.
    for _ in 0..2 {
        let report = tracker.tick().await;
        assert_eq!(report.skipped, 1);
        assert_eq!(report.synced, 0);
        let status = repo
            .get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .unwrap();
        assert_eq!(status.state, PrState::ReviewPending);
    }
    assert_eq!(pr_events(&repo, &session.id).await.len(), 1);

    // Third consecutive failure crosses the threshold.
    let report = tracker.tick().await;
    assert_eq!(report.synced, 1);
    let status = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(status.state, PrState::SyncError);
    let events = pr_events(&repo, &session.id).await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1],
        serde_json::json!({"type": "pr_state_changed", "from": "review_pending", "to": "sync_error"})
    );

    // sync_error is not terminal: a readable PR brings the state back.
    server.reset().await;
    mocks(&server)
        .open_pr(
            "sha1",
            vec![review_json(1, "ana", "APPROVED", "sha1", "")],
            vec![],
        )
        .await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    tracker.tick().await;
    let status = repo
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(status.state, PrState::Approved);
    assert_eq!(
        pr_events(&repo, &session.id).await.last().unwrap()["from"],
        "sync_error"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn transient_errors_skip_without_counting_toward_sync_error() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    mocks(&server).pull_request_status(502).await;
    let tracker = tracker(&db, &server);

    for _ in 0..4 {
        let report = tracker.tick().await;
        assert_eq!(report.skipped, 1);
    }
    assert!(
        repo.get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .is_none()
    );

    server.reset().await;
    mocks(&server).open_pr("sha1", vec![], vec![]).await;
    tracker.tick().await;
    assert_eq!(
        repo.get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .unwrap()
            .state,
        PrState::ReviewPending
    );

    db.cleanup().await;
}

#[tokio::test]
async fn rate_limit_backs_off_the_whole_tick() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let a = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(PR_URL, "vise/widgets")),
        0,
    )
    .await;
    let b = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened(
            "https://github.com/acme/widgets/pull/8",
            "vise/b",
        )),
        0,
    )
    .await;

    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .respond_with(wiremock::ResponseTemplate::new(429).insert_header("retry-after", "90"))
        .mount(&server)
        .await;

    let report = tracker(&db, &server).tick().await;
    assert_eq!(report.backoff, Some(Duration::from_secs(90)));
    assert_eq!(report.synced, 0);
    // Only one PR was attempted; the tick stopped at the first rate limit.
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    for id in [&a.id, &b.id] {
        assert!(repo.get(id).await.unwrap().unwrap().pr_status.is_none());
        assert!(pr_events(&repo, id).await.is_empty());
    }

    db.cleanup().await;
}

#[tokio::test]
async fn only_pr_opened_outcomes_are_polled() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let pushed = SessionOutcome {
        kind: "pushed_no_pr".into(),
        pr_url: None,
        branch: Some("x".into()),
    };
    let updated = SessionOutcome {
        kind: "pr_updated".into(),
        pr_url: Some(PR_URL.into()),
        branch: Some("x".into()),
    };
    finished_session(&repo, github_env(REPO), Some(pushed), 0).await;
    finished_session(&repo, github_env(REPO), Some(updated), 0).await;
    finished_session(&repo, github_env(REPO), None, 0).await;

    let report = tracker(&db, &server).tick().await;
    assert_eq!(report, TickReport::default());
    assert!(server.received_requests().await.unwrap().is_empty());

    db.cleanup().await;
}

#[tokio::test]
async fn unparseable_pr_url_is_a_sync_error() {
    let Some(db) = test_db().await else { return };
    let repo = db.repository();
    let server = MockServer::start().await;

    let session = finished_session(
        &repo,
        github_env(REPO),
        Some(pr_opened("https://example.com/not-a-pr", "vise/widgets")),
        0,
    )
    .await;

    tracker(&db, &server).tick().await;
    assert_eq!(
        repo.get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .unwrap()
            .state,
        PrState::SyncError
    );
    assert!(server.received_requests().await.unwrap().is_empty());

    db.cleanup().await;
}
