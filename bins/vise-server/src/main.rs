use std::sync::Arc;

use sqlx::postgres::PgPoolOptions;
use vise_api::{AppState, app};
use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};

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

    let sessions = Arc::new(SessionService::new(PostgresSessionRepository::new(
        pool.clone(),
    )));
    let hosts = Arc::new(HostService::new(PostgresHostRepository::new(pool)));

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

    if let (Ok(app_id), Ok(key_path)) = (
        std::env::var("VISE_GITHUB_APP_ID"),
        std::env::var("VISE_GITHUB_APP_PRIVATE_KEY_PATH"),
    ) {
        let pem = std::fs::read_to_string(&key_path)?;
        let client = vise_api::github::GitHubAppClient::new(
            app_id.parse()?,
            &pem,
            "https://api.github.com".to_string(),
        )?;
        credentials.insert(
            "github".to_string(),
            Arc::new(vise_api::credentials::GithubCredentialProvider { client }),
        );
        tracing::info!(%app_id, "github credential provider configured");
    } else {
        tracing::warn!("github app not configured; github_repo sessions will fail");
    }

    let state = AppState {
        sessions,
        hosts,
        credentials,
    };

    let app = app(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;

    tracing::info!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app).await?;

    Ok(())
}
