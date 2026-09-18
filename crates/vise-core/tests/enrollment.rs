//! Enrollment tokens at the repository/service seam: exchange semantics
//! (workspace binding, ephemeral flag, revocation, max_uses — including the
//! concurrent race), and the ephemeral-host reaper. These tests need
//! `DATABASE_URL`; `sqlx::test` creates a throwaway database per test.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use vise_core::enrollment::postgres::PostgresEnrollmentTokenRepository;
use vise_core::enrollment::service::EnrollmentTokenService;
use vise_core::hosts::postgres::PostgresHostRepository;
use vise_core::hosts::service::HostService;
use vise_core::sessions::model::{Agent, Environment, Session, SessionStatus};
use vise_core::sessions::postgres::PostgresSessionRepository;
use vise_core::sessions::repository::SessionRepository;
use vise_core::workspaces::model::{Workspace, WorkspaceId};
use vise_core::workspaces::postgres::PostgresWorkspaceRepository;
use vise_core::workspaces::repository::WorkspaceRepository;

type Enrollment = EnrollmentTokenService<PostgresEnrollmentTokenRepository, PostgresHostRepository>;

fn services(pool: &PgPool) -> (Arc<HostService<PostgresHostRepository>>, Enrollment) {
    let hosts = Arc::new(HostService::new(PostgresHostRepository::new(pool.clone())));
    let enrollment = EnrollmentTokenService::new(
        PostgresEnrollmentTokenRepository::new(pool.clone()),
        hosts.clone(),
    );
    (hosts, enrollment)
}

async fn create_workspace(pool: &PgPool, id: &str) -> WorkspaceId {
    let workspaces = PostgresWorkspaceRepository::new(pool.clone());
    workspaces
        .create(Workspace {
            id: WorkspaceId::new(id),
            name: id.to_string(),
            settings: serde_json::json!({}),
            created_at: Utc::now(),
        })
        .await
        .unwrap();
    WorkspaceId::new(id)
}

async fn create_session(pool: &PgPool, workspace: &WorkspaceId) -> Session {
    let now = Utc::now();
    PostgresSessionRepository::new(pool.clone())
        .create(Session {
            id: vise_core::id::new_id("ses"),
            workspace_id: workspace.clone(),
            agent: Agent {
                harness: "echo".into(),
                model: String::new(),
                instructions: String::new(),
                mcp_servers: vec![],
            },
            environment: Environment {
                kind: "self_hosted".into(),
                repo: None,
                base_branch: None,
            },
            input: "do the thing".into(),
            status: SessionStatus::Pending,
            host_id: None,
            lease_expires_at: None,
            started_at: None,
            finished_at: None,
            stop_reason: None,
            error: None,
            outcome: None,
            pr_status: None,
            parent_session_id: None,
            cancel_requested: false,
            created_at: now,
            updated_at: now,
        })
        .await
        .unwrap()
}

/// Push a host's clock back so the reaper's cutoff passes it.
async fn age_host(pool: &PgPool, host_id: &str, seconds: i64) {
    sqlx::query("UPDATE hosts SET last_seen_at = now() - make_interval(secs => $2) WHERE id = $1")
        .bind(host_id)
        .bind(seconds as f64)
        .execute(pool)
        .await
        .unwrap();
}

// --- mint / list / revoke ---------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn mint_returns_the_secret_once_and_lists_without_it(pool: PgPool) {
    let (_, enrollment) = services(&pool);
    let acme = create_workspace(&pool, "ws_acme").await;

    let minted = enrollment.mint(acme.clone(), Some(5)).await.unwrap();
    assert!(minted.secret.starts_with("venroll_"), "{}", minted.secret);
    assert_eq!(minted.token.workspace_id, acme);
    assert_eq!(minted.token.max_uses, Some(5));
    assert_eq!(minted.token.uses, 0);
    assert!(minted.token.revoked_at.is_none());

    // Listing is workspace-scoped and carries no secret material at all.
    let listed = enrollment.list(&acme).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, minted.token.id);
    assert!(
        enrollment
            .list(&WorkspaceId::DEFAULT)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn mint_rejects_a_nonpositive_use_cap(pool: PgPool) {
    let (_, enrollment) = services(&pool);
    assert!(
        enrollment
            .mint(WorkspaceId::DEFAULT, Some(0))
            .await
            .is_err()
    );
    assert!(
        enrollment
            .mint(WorkspaceId::DEFAULT, Some(-1))
            .await
            .is_err()
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn revoke_is_idempotent_and_workspace_scoped(pool: PgPool) {
    let (_, enrollment) = services(&pool);
    let acme = create_workspace(&pool, "ws_acme").await;
    let minted = enrollment.mint(acme.clone(), None).await.unwrap();

    // The wrong workspace cannot see the token, let alone revoke it.
    assert!(
        enrollment
            .revoke(&WorkspaceId::DEFAULT, &minted.token.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        enrollment
            .exchange(&minted.secret, None)
            .await
            .unwrap()
            .is_some()
    );

    let revoked = enrollment
        .revoke(&acme, &minted.token.id)
        .await
        .unwrap()
        .unwrap();
    let revoked_at = revoked.revoked_at.expect("revoked_at set");

    // Revoking again keeps the original timestamp.
    let again = enrollment
        .revoke(&acme, &minted.token.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.revoked_at, Some(revoked_at));

    assert!(
        enrollment
            .exchange(&minted.secret, None)
            .await
            .unwrap()
            .is_none(),
        "a revoked token must not enroll hosts"
    );
}

// --- exchange ---------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn exchange_enrolls_an_ephemeral_host_in_the_tokens_workspace(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let acme = create_workspace(&pool, "ws_acme").await;
    let minted = enrollment.mint(acme.clone(), None).await.unwrap();

    let enrolled = enrollment
        .exchange(&minted.secret, Some("gpu"))
        .await
        .unwrap()
        .expect("valid secret exchanges");

    assert_eq!(enrolled.host.workspace_id, acme);
    assert!(enrolled.host.ephemeral);
    assert!(
        enrolled.host.name.starts_with("gpu-"),
        "{}",
        enrolled.host.name
    );
    assert!(enrolled.token.starts_with("vhost_"), "{}", enrolled.token);

    // The vhost_ token authenticates as that host, like a hand-enrolled one.
    let authed = hosts.authenticate(&enrolled.token).await.unwrap().unwrap();
    assert_eq!(authed.id, enrolled.host.id);
    assert!(authed.ephemeral);

    // The exchange counted as a use.
    let listed = enrollment.list(&acme).await.unwrap();
    assert_eq!(listed[0].uses, 1);
}

#[sqlx::test(migrations = "./migrations")]
async fn exchange_defaults_the_name_prefix_and_generates_unique_names(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let minted = enrollment.mint(WorkspaceId::DEFAULT, None).await.unwrap();

    let first = enrollment
        .exchange(&minted.secret, None)
        .await
        .unwrap()
        .unwrap();
    let second = enrollment
        .exchange(&minted.secret, Some("  "))
        .await
        .unwrap()
        .unwrap();

    assert!(first.host.name.starts_with("host-"), "{}", first.host.name);
    assert!(
        second.host.name.starts_with("host-"),
        "{}",
        second.host.name
    );
    assert_ne!(first.host.name, second.host.name);
    assert_eq!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn exchange_rejects_an_unknown_secret(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    assert!(
        enrollment
            .exchange("venroll_never_minted", None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().is_empty());
}

#[sqlx::test(migrations = "./migrations")]
async fn exchange_stops_exactly_at_max_uses(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let minted = enrollment
        .mint(WorkspaceId::DEFAULT, Some(2))
        .await
        .unwrap();

    for _ in 0..2 {
        assert!(
            enrollment
                .exchange(&minted.secret, None)
                .await
                .unwrap()
                .is_some()
        );
    }
    assert!(
        enrollment
            .exchange(&minted.secret, None)
            .await
            .unwrap()
            .is_none(),
        "the exchange after max_uses must be rejected"
    );

    let listed = enrollment.list(&WorkspaceId::DEFAULT).await.unwrap();
    assert_eq!(listed[0].uses, 2, "uses must not pass max_uses");
    assert_eq!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().len(), 2);
}

#[sqlx::test(migrations = "./migrations")]
async fn concurrent_exchanges_cannot_race_past_max_uses(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let enrollment = Arc::new(enrollment);
    let minted = enrollment
        .mint(WorkspaceId::DEFAULT, Some(3))
        .await
        .unwrap();

    let attempts: Vec<_> = (0..12)
        .map(|_| {
            let enrollment = enrollment.clone();
            let secret = minted.secret.clone();
            tokio::spawn(async move { enrollment.exchange(&secret, None).await.unwrap() })
        })
        .collect();

    let mut enrolled = 0;
    for attempt in attempts {
        if attempt.await.unwrap().is_some() {
            enrolled += 1;
        }
    }

    assert_eq!(enrolled, 3, "exactly max_uses exchanges may win");
    let listed = enrollment.list(&WorkspaceId::DEFAULT).await.unwrap();
    assert_eq!(listed[0].uses, 3);
    assert_eq!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().len(), 3);
}

// --- reaper -----------------------------------------------------------------

#[sqlx::test(migrations = "./migrations")]
async fn reaper_deletes_only_stale_ephemeral_hosts(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let minted = enrollment.mint(WorkspaceId::DEFAULT, None).await.unwrap();

    let stale = enrollment
        .exchange(&minted.secret, Some("stale"))
        .await
        .unwrap()
        .unwrap();
    let fresh = enrollment
        .exchange(&minted.secret, Some("fresh"))
        .await
        .unwrap()
        .unwrap();
    let permanent = hosts
        .enroll(WorkspaceId::DEFAULT, "hand-enrolled".into())
        .await
        .unwrap();

    // stale: last seen two hours ago; permanent: equally old but not
    // ephemeral; fresh: just checked in.
    age_host(&pool, &stale.host.id, 7200).await;
    age_host(&pool, &permanent.host.id, 7200).await;
    hosts.authenticate(&fresh.token).await.unwrap().unwrap();

    let reaped = hosts
        .reap_ephemeral(Duration::from_secs(3600))
        .await
        .unwrap();
    assert_eq!(reaped, 1);

    let names: Vec<String> = hosts
        .list(&WorkspaceId::DEFAULT)
        .await
        .unwrap()
        .into_iter()
        .map(|host| host.name)
        .collect();
    assert!(!names.iter().any(|name| name.starts_with("stale-")));
    assert!(names.iter().any(|name| name.starts_with("fresh-")));
    assert!(
        names.iter().any(|name| name == "hand-enrolled"),
        "a non-ephemeral host must never be reaped: {names:?}"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reaper_counts_never_seen_hosts_from_enrollment(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let minted = enrollment.mint(WorkspaceId::DEFAULT, None).await.unwrap();

    // Enrolled but never checked in: last_seen_at is NULL, so the TTL runs
    // from created_at instead.
    enrollment
        .exchange(&minted.secret, None)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        hosts
            .reap_ephemeral(Duration::from_secs(3600))
            .await
            .unwrap(),
        0,
        "a just-enrolled host is within the TTL"
    );
    assert_eq!(
        hosts.reap_ephemeral(Duration::ZERO).await.unwrap(),
        1,
        "with a zero TTL the never-seen host is stale"
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn reaper_spares_a_host_holding_a_running_session(pool: PgPool) {
    let (hosts, enrollment) = services(&pool);
    let sessions = PostgresSessionRepository::new(pool.clone());
    let minted = enrollment.mint(WorkspaceId::DEFAULT, None).await.unwrap();

    let enrolled = enrollment
        .exchange(&minted.secret, None)
        .await
        .unwrap()
        .unwrap();
    let session = create_session(&pool, &WorkspaceId::DEFAULT).await;
    let claimed = sessions
        .claim_pending(&enrolled.host.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, session.id);

    age_host(&pool, &enrolled.host.id, 7200).await;
    assert_eq!(
        hosts
            .reap_ephemeral(Duration::from_secs(3600))
            .await
            .unwrap(),
        0,
        "the lease sweeper, not the reaper, decides a running session's fate"
    );

    // Once the session is no longer running, the next pass reaps the host.
    sessions
        .finish(
            &enrolled.host.id,
            &session.id,
            SessionStatus::Failed,
            None,
            Some("host went away".into()),
            None,
        )
        .await
        .unwrap()
        .unwrap();
    age_host(&pool, &enrolled.host.id, 7200).await;
    assert_eq!(
        hosts
            .reap_ephemeral(Duration::from_secs(3600))
            .await
            .unwrap(),
        1
    );
    assert!(hosts.list(&WorkspaceId::DEFAULT).await.unwrap().is_empty());
}
