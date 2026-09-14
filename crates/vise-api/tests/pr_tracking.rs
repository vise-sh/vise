mod common;

use std::time::Duration;

use common::*;
use sqlx::PgPool;
use vise_api::pr_tracking::{PrPoller, SYNC_ERROR_THRESHOLD};
use vise_core::sessions::model::{ChecksState, PrState, SessionOutcome};
use vise_core::sessions::postgres::PostgresSessionRepository;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn poller(state: &vise_api::AppState) -> PrPoller<PostgresSessionRepository> {
    PrPoller::new(
        state.sessions.clone(),
        state.github.clone(),
        Duration::from_secs(60),
    )
}

async fn pr_events(state: &vise_api::AppState, id: &str) -> Vec<(i64, serde_json::Value)> {
    state
        .sessions
        .get_events(id, 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.payload.get("type").is_some())
        .map(|e| (e.seq, e.payload))
        .collect()
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn first_sync_writes_snapshot_and_transition_events_together(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;

    mount_pull(&server, 17, pull_json(true, false, "vise/feature", "sha1")).await;
    mount_reviews(
        &server,
        17,
        serde_json::json!([
            review("alice", "APPROVED", "sha1", 1),
            review("bob", "CHANGES_REQUESTED", "sha1", 2),
        ]),
    )
    .await;
    mount_check_runs(
        &server,
        "sha1",
        serde_json::json!([
            check_run("test", "completed", Some("failure")),
            check_run("lint", "in_progress", None),
        ]),
    )
    .await;

    let report = poller(&state).tick().await;
    assert_eq!(report.synced, 1);
    assert_eq!(report.backoff, None);

    let synced = state.sessions.get(&session.id).await.unwrap().unwrap();
    let status = synced.pr_status.expect("snapshot written");
    assert_eq!(status.state, PrState::ChangesRequested);
    assert_eq!(status.checks, Some(ChecksState::Failing));

    // Events continue the host's seq space (the host wrote seq 1 and 2).
    let events = pr_events(&state, &session.id).await;
    assert_eq!(
        events,
        vec![
            (
                3,
                serde_json::json!({ "type": "pr_state_changed", "from": null, "to": "changes_requested" })
            ),
            (
                4,
                serde_json::json!({ "type": "checks_state_changed", "from": null, "to": "failing" })
            ),
        ]
    );
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn unchanged_observation_touches_last_synced_at_only(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;
    mount_open_pr(&server, 17).await;

    let mut poller = poller(&state);
    poller.tick().await;
    let first = state
        .sessions
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    let events_after_first = pr_events(&state, &session.id).await;
    assert_eq!(events_after_first.len(), 2, "initial state + checks");

    tokio::time::sleep(Duration::from_millis(5)).await;
    let report = poller.tick().await;
    assert_eq!(report.synced, 1);

    let second = state
        .sessions
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .unwrap();
    assert_eq!(second.state, first.state);
    assert_eq!(second.checks, first.checks);
    assert!(second.last_synced_at > first.last_synced_at);
    assert_eq!(
        pr_events(&state, &session.id).await,
        events_after_first,
        "no event on a quiet tick"
    );
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn only_the_changed_dimension_emits_an_event(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;

    mount_pull(&server, 17, pull_json(true, false, "vise/feature", "sha1")).await;
    mount_reviews(&server, 17, serde_json::json!([])).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/commits/sha1/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [check_run("test", "queued", None)]
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_check_runs(
        &server,
        "sha1",
        serde_json::json!([check_run("test", "completed", Some("success"))]),
    )
    .await;

    let mut poller = poller(&state);
    poller.tick().await;
    poller.tick().await;

    let events = pr_events(&state, &session.id).await;
    let payloads: Vec<&serde_json::Value> = events.iter().map(|(_, p)| p).collect();
    assert_eq!(
        payloads,
        vec![
            &serde_json::json!({ "type": "pr_state_changed", "from": null, "to": "review_pending" }),
            &serde_json::json!({ "type": "checks_state_changed", "from": null, "to": "pending" }),
            &serde_json::json!({ "type": "checks_state_changed", "from": "pending", "to": "passing" }),
        ]
    );
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn merged_and_closed_prs_leave_the_work_list(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let merged = pr_session(&state, 1).await;
    let closed = pr_session(&state, 2).await;
    let open = pr_session(&state, 3).await;

    // Sessions without a PR are never on the work list.
    finished_session(
        &state,
        SessionOutcome {
            kind: "pushed_no_pr".into(),
            pr_url: None,
            branch: Some("vise/x".into()),
        },
        0,
    )
    .await;

    mount_pull(&server, 1, pull_json(false, true, "vise/a", "sha1")).await;
    mount_pull(&server, 2, pull_json(false, false, "vise/b", "sha1")).await;
    mount_pull(&server, 3, pull_json(true, false, "vise/c", "sha1")).await;
    for number in 1..=3 {
        mount_reviews(&server, number, serde_json::json!([])).await;
    }
    mount_check_runs(&server, "sha1", serde_json::json!([])).await;

    let work_before = state.sessions.pr_tracking_work_list(100).await.unwrap();
    assert_eq!(work_before.len(), 3);

    let mut poller = poller(&state);
    let report = poller.tick().await;
    assert_eq!(report.synced, 3);

    let state_of = |id: String| {
        let sessions = state.sessions.clone();
        async move {
            sessions
                .get(&id)
                .await
                .unwrap()
                .unwrap()
                .pr_status
                .unwrap()
                .state
        }
    };
    assert_eq!(state_of(merged.id.clone()).await, PrState::Merged);
    assert_eq!(state_of(closed.id.clone()).await, PrState::Closed);
    assert_eq!(state_of(open.id.clone()).await, PrState::ReviewPending);

    let work_after = state.sessions.pr_tracking_work_list(100).await.unwrap();
    assert_eq!(
        work_after.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        vec![open.id.as_str()]
    );

    // Terminal sessions are never synced again.
    let report = poller.tick().await;
    assert_eq!(report.synced, 1);
    assert!(
        state
            .sessions
            .begin_pr_sync(&merged.id)
            .await
            .unwrap()
            .is_none()
    );
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn persistent_not_found_becomes_sync_error_and_recovers(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/17"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(SYNC_ERROR_THRESHOLD as u64)
        .mount(&server)
        .await;
    mount_open_pr(&server, 17).await;

    let mut poller = poller(&state);

    for attempt in 1..SYNC_ERROR_THRESHOLD {
        let report = poller.tick().await;
        assert_eq!(report.skipped, 1, "attempt {attempt} skips");
        let snapshot = state.sessions.get(&session.id).await.unwrap().unwrap();
        assert!(
            snapshot.pr_status.is_none(),
            "attempt {attempt} stays quiet"
        );
    }

    let report = poller.tick().await;
    assert_eq!(report.synced, 1);
    let snapshot = state.sessions.get(&session.id).await.unwrap().unwrap();
    assert_eq!(snapshot.pr_status.unwrap().state, PrState::SyncError);
    assert_eq!(
        pr_events(&state, &session.id).await.last().unwrap().1,
        serde_json::json!({ "type": "pr_state_changed", "from": null, "to": "sync_error" })
    );

    // sync_error is not terminal: the next successful fetch clears it.
    let work = state.sessions.pr_tracking_work_list(100).await.unwrap();
    assert_eq!(work.len(), 1);
    poller.tick().await;
    let snapshot = state.sessions.get(&session.id).await.unwrap().unwrap();
    assert_eq!(snapshot.pr_status.unwrap().state, PrState::Approved);
    assert_eq!(
        pr_events(&state, &session.id).await.last().unwrap().1,
        serde_json::json!({ "type": "checks_state_changed", "from": null, "to": "passing" })
    );
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn transient_failures_skip_and_retry(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;

    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/17"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    mount_open_pr(&server, 17).await;

    let mut poller = poller(&state);
    let report = poller.tick().await;
    assert_eq!((report.synced, report.skipped), (0, 1));
    assert!(
        state
            .sessions
            .get(&session.id)
            .await
            .unwrap()
            .unwrap()
            .pr_status
            .is_none()
    );

    let report = poller.tick().await;
    assert_eq!((report.synced, report.skipped), (1, 0));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn rate_limit_backs_off_the_whole_tick(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    pr_session(&state, 1).await;
    pr_session(&state, 2).await;

    let reset = chrono::Utc::now() + chrono::Duration::seconds(120);
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .insert_header("x-ratelimit-reset", reset.timestamp().to_string().as_str()),
        )
        .expect(1)
        .mount(&server)
        .await;

    let report = poller(&state).tick().await;
    assert_eq!(report.synced, 0);
    assert_eq!(report.skipped, 1, "the rest of the list is left for later");
    let backoff = report.backoff.expect("backoff requested");
    assert!(backoff > Duration::from_secs(100) && backoff <= Duration::from_secs(120));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn a_locked_session_is_skipped_by_a_second_poller(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;
    mount_open_pr(&server, 17).await;

    let held = state
        .sessions
        .begin_pr_sync(&session.id)
        .await
        .unwrap()
        .expect("first lock succeeds");

    let report = poller(&state).tick().await;
    assert_eq!((report.synced, report.skipped), (0, 1));

    drop(held);
    let report = poller(&state).tick().await;
    assert_eq!((report.synced, report.skipped), (1, 0));
}

#[sqlx::test(migrations = "../vise-core/migrations")]
async fn pat_auth_tracks_prs_against_the_configured_api_base(pool: PgPool) {
    let server = MockServer::start().await;
    let state = app_state(pool, &server.uri(), pat_auth());
    let session = pr_session(&state, 17).await;

    // Every read carries the PAT; nothing else would match these mocks.
    let bearer = || header("authorization", format!("Bearer {TOKEN}").as_str());
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/17"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(pull_json(
            true,
            false,
            "vise/feature",
            "sha1",
        )))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/pulls/17/reviews"))
        .and(bearer())
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!([review("alice", "APPROVED", "sha1", 1)])),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/commits/sha1/check-runs"))
        .and(bearer())
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "total_count": 1,
            "check_runs": [check_run("test", "completed", Some("success"))]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let report = poller(&state).tick().await;
    assert_eq!((report.synced, report.skipped), (1, 0));

    let status = state
        .sessions
        .get(&session.id)
        .await
        .unwrap()
        .unwrap()
        .pr_status
        .expect("snapshot written");
    assert_eq!(status.state, PrState::Approved);
    assert_eq!(status.checks, Some(ChecksState::Passing));
    assert_eq!(
        pr_events(&state, &session.id)
            .await
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>(),
        vec![
            serde_json::json!({ "type": "pr_state_changed", "from": null, "to": "approved" }),
            serde_json::json!({ "type": "checks_state_changed", "from": null, "to": "passing" }),
        ]
    );
}
