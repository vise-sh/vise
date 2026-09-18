use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use vise_api::{AppState, app};
use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};
use vise_core::workspaces::model::WorkspaceId;
use vise_core::workspaces::postgres::PostgresWorkspaceRepository;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url = std::env::var("DATABASE_URL")?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await?;

    // Apply pending schema migrations before serving. The migrations are
    // embedded in vise-core, so a container or release binary needs no
    // sqlx-cli; `just db-migrate` keeps working because both write the same
    // `_sqlx_migrations` table.
    vise_core::MIGRATOR.run(&pool).await?;
    tracing::info!("database migrations applied");

    let sessions = Arc::new(SessionService::new(PostgresSessionRepository::new(
        pool.clone(),
    )));
    let hosts = Arc::new(HostService::new(PostgresHostRepository::new(pool.clone())));
    let workspaces = Arc::new(PostgresWorkspaceRepository::new(pool));

    // Lease-expiry sweeper: hosts that crash stop heartbeating, so their
    // running sessions are failed once the lease lapses.
    {
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
            loop {
                interval.tick().await;
                match sessions.expire_leases().await {
                    Ok(0) => {}
                    Ok(count) => tracing::warn!(count, "expired session leases"),
                    Err(error) => tracing::error!(%error, "lease sweeper failed"),
                }
            }
        });
    }

    let mut credentials: std::collections::HashMap<
        String,
        Arc<dyn vise_api::credentials::CredentialProvider>,
    > = std::collections::HashMap::new();

    let github_api_base = std::env::var("VISE_GITHUB_API_BASE")
        .unwrap_or_else(|_| "https://api.github.com".to_string());

    // GitHub auth: the App when configured, otherwise a PAT. The same source
    // backs the tokens handed to hosts and the server's own PR reads.
    let github_app = match (
        env_value("VISE_GITHUB_APP_ID"),
        env_value("VISE_GITHUB_APP_PRIVATE_KEY_PATH"),
    ) {
        (Some(app_id), Some(key_path)) => {
            let pem = std::fs::read_to_string(&key_path)?;
            let client = vise_api::github::GitHubAppClient::new(
                app_id.parse()?,
                &pem,
                github_api_base.clone(),
            )?;
            tracing::info!(%app_id, "github app configured");
            Some(Arc::new(client))
        }
        _ => None,
    };
    let github_pat = env_value("VISE_GITHUB_PAT");
    let github = vise_api::github::GithubAuth::select(github_app, github_pat).map(|auth| {
        tracing::info!(auth = auth.kind(), "github credential provider configured");
        credentials.insert("github".to_string(), auth.credential_provider());
        vise_api::github::GitHubApi::new(github_api_base, auth)
    });
    if github.is_none() {
        tracing::warn!("github app or pat not configured; github_repo sessions will fail");
    }

    // PR tracker: follows every PR a session opened until it merges or closes,
    // recording state transitions as session events. The credential needs
    // "Pull requests: read" and "Checks: read" (GitHub App) or "Pull requests:
    // read" and "Actions: read" (fine-grained PAT) on the tracked repositories.
    {
        let interval_secs: u64 = std::env::var("VISE_PR_POLL_INTERVAL_SECS")
            .ok()
            .map(|value| value.parse())
            .transpose()?
            .unwrap_or(60);
        let poller = vise_api::pr_tracking::PrPoller::new(
            sessions.clone(),
            github.clone(),
            std::time::Duration::from_secs(interval_secs.max(1)),
        );
        tokio::spawn(poller.run_forever());
    }

    // Single-tenant: every host and session lives in the `default`
    // workspace the migrations seed.
    let state = AppState {
        sessions,
        hosts,
        workspaces,
        workspace: WorkspaceId::DEFAULT,
        credentials,
        github,
    };

    let app = app(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;

    tracing::info!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;

    Ok(())
}

/// An environment variable, treating unset and blank the same way.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
