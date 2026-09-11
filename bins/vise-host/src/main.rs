mod acp;
mod github;
mod runtime;

use std::time::Duration;

use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vise_client::{
    Client as ViseClient,
    types::{ClaimRequest, FinishRequest, IssueCredentialRequest, NewSessionEvent,
        ReportEventsRequest, Session, SessionOutcome, SessionStatus},
};

use acp::AcpProcessRuntime;
use runtime::{EchoRuntime, RunOutcome, SessionRuntime};

#[derive(Parser)]
#[command(name = "vise-host")]
#[command(version)]
#[command(about = "Vise session host")]
struct Cli {
    /// URL of the Vise API
    #[arg(short, long, default_value = "http://localhost:3000")]
    url: String,

    /// Host bearer token from enrollment (vise hosts create <name>)
    #[arg(long, env = "VISE_HOST_TOKEN")]
    token: String,

    /// Claim poll interval in seconds
    #[arg(long, default_value_t = 2)]
    poll_interval: u64,

    /// Keep session workspaces on disk after the session finishes
    #[arg(long, default_value_t = false)]
    keep_workspaces: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    let mut headers = reqwest::header::HeaderMap::new();
    let mut auth = reqwest::header::HeaderValue::try_from(format!("Bearer {}", cli.token))?;
    auth.set_sensitive(true);
    headers.insert(reqwest::header::AUTHORIZATION, auth);

    let http = reqwest::Client::builder().default_headers(headers).build()?;
    let client = ViseClient::new_with_client(&cli.url, http);

    let echo = EchoRuntime;
    let acp = AcpProcessRuntime;

    clean_orphaned_workspaces(&client).await;

    tracing::info!(url = %cli.url, "polling for work");

    loop {
        match claim(&client).await {
            Ok(Some(session)) => {
                tracing::info!(session_id = %session.id, harness = %session.agent.harness, "claimed session");

                let runtime: &dyn SessionRuntime = if session.agent.harness == "echo" {
                    &echo
                } else {
                    &acp
                };

                if let Err(error) =
                    run_session(&client, runtime, session, cli.keep_workspaces).await
                {
                    tracing::error!(%error, "session run failed");
                }
            }

            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(cli.poll_interval)).await;
            }

            Err(error) => {
                tracing::warn!(%error, "claim failed");
                tokio::time::sleep(Duration::from_secs(cli.poll_interval)).await;
            }
        }
    }
}

/// Remove leftover workspaces from previous runs whose sessions are terminal
/// (or unknown to the server). Workspaces marked with a `.keep` file are
/// intentionally retained and left alone.
async fn clean_orphaned_workspaces(client: &ViseClient) {
    let root = std::env::temp_dir().join("vise-sessions");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.join(".keep").exists() {
            continue;
        }
        let Some(session_id) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };

        let removable = match client.get_session(&session_id).await {
            Ok(response) => matches!(
                response.into_inner().status,
                SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
            ),
            // Unknown session: nothing will ever finish it; the dir is garbage.
            Err(error) => error.status().is_some_and(|s| s.as_u16() == 404),
        };

        if removable {
            tracing::info!(%session_id, "removing orphaned workspace");
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

async fn claim(client: &ViseClient) -> anyhow::Result<Option<Session>> {
    let response = client
        .claim_session(&ClaimRequest {
            harnesses: vec!["claude-code".to_string()],
            environment_types: vec!["self_hosted".to_string(), "github_repo".to_string()],
        })
        .await?;

    Ok(response.into_inner().session)
}

async fn run_session(
    client: &ViseClient,
    runtime: &dyn SessionRuntime,
    mut session: Session,
    keep_workspaces: bool,
) -> anyhow::Result<()> {
    let workdir = std::env::temp_dir().join("vise-sessions").join(&session.id);
    let workspace = workdir.join("workspace");

    let cancel = CancellationToken::new();

    // Heartbeat before preparation: the claim lease is short (60s), so a slow
    // clone must not run without heartbeats extending the lease.
    let heartbeat = tokio::spawn(heartbeat_loop(
        client.clone(),
        session.id.clone(),
        cancel.clone(),
    ));

    let github_ctx: Option<(github::PreparedRepo, String, String)> =
        if session.environment.kind == "github_repo" {
            match prepare_github(client, &mut session, &workdir).await {
                Ok(ctx) => Some(ctx),
                Err(error) => {
                    heartbeat.abort();
                    tracing::error!(session_id = %session.id, %error, "workspace preparation failed");
                    client
                        .finish_session(
                            &session.id,
                            &FinishRequest {
                                status: SessionStatus::Failed,
                                stop_reason: None,
                                error: Some(error.to_string()),
                                outcome: None,
                            },
                        )
                        .await?;
                    if !keep_workspaces {
                        let _ = std::fs::remove_dir_all(&workdir);
                    } else {
                        // Marker so startup orphan-cleanup leaves this workspace alone.
                        let _ = std::fs::write(workdir.join(".keep"), "");
                    }
                    return Ok(());
                }
            }
        } else {
            if let Err(error) = std::fs::create_dir_all(&workspace) {
                heartbeat.abort();
                return Err(error.into());
            }
            None
        };

    let (events_tx, events_rx) = mpsc::channel::<serde_json::Value>(256);

    let uploader = tokio::spawn(upload_events(client.clone(), session.id.clone(), events_rx));

    let refresher = github_ctx.as_ref().map(|(prepared, _, _)| {
        tokio::spawn(refresh_token_loop(
            client.clone(),
            session.id.clone(),
            prepared.token_file.clone(),
        ))
    });

    let (run_workspace, gh_token) = match &github_ctx {
        Some((prepared, _, secret)) => (prepared.repo_dir.clone(), Some(secret.clone())),
        None => (workspace.clone(), None),
    };

    let run_result = runtime
        .run(&session, &run_workspace, gh_token, events_tx, cancel.clone())
        .await;

    // events_tx is dropped by run; the uploader drains and flushes what's left.
    // Capture the result so the background tasks are always torn down.
    let upload_result = uploader.await;
    heartbeat.abort();
    if let Some(refresher) = &refresher {
        refresher.abort();
    }
    match upload_result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "event upload gave up"),
        Err(error) => tracing::warn!(%error, "event uploader task panicked"),
    }

    let run_ok = run_result.is_ok();
    let (status, stop_reason, error) = match run_result {
        Ok(RunOutcome::Completed { stop_reason }) => {
            (SessionStatus::Completed, Some(stop_reason), None)
        }
        Ok(RunOutcome::Cancelled) => (SessionStatus::Cancelled, None, None),
        Err(error) => (SessionStatus::Failed, None, Some(error.to_string())),
    };

    let mut outcome: Option<SessionOutcome> = None;
    if let Some((prepared, repo, _)) = &github_ctx
        && run_ok
    {
        let token = std::fs::read_to_string(&prepared.token_file).ok();
        let pr_lookup = token.as_deref().map(|t| (repo.as_str(), t));
        match github::detect_outcome(prepared, pr_lookup).await {
            Ok(detected) => {
                outcome = Some(SessionOutcome {
                    kind: detected.kind,
                    pr_url: detected.pr_url,
                    branch: detected.branch,
                });
            }
            Err(error) => tracing::warn!(session_id = %session.id, %error, "outcome detection failed"),
        }
    }

    tracing::info!(session_id = %session.id, ?status, "finishing session");

    let keep = keep_workspaces
        || outcome.as_ref().is_some_and(|o| o.kind == "uncommitted_changes");

    // Capture the result so workspace cleanup runs even when finishing fails.
    let finish_result = client
        .finish_session(
            &session.id,
            &FinishRequest {
                status,
                stop_reason,
                error,
                outcome,
            },
        )
        .await;

    if let Some((prepared, _, _)) = &github_ctx {
        if !keep {
            let _ = std::fs::remove_dir_all(&workdir);
        } else {
            // Kept workspaces must not retain the (short-lived) token file.
            let _ = std::fs::remove_file(&prepared.token_file);
            // Marker so startup orphan-cleanup leaves this workspace alone.
            let _ = std::fs::write(workdir.join(".keep"), "");
            tracing::info!(path = %workdir.display(), "keeping workspace");
        }
    } else if keep {
        let _ = std::fs::write(workdir.join(".keep"), "");
    }

    finish_result?;
    Ok(())
}

/// Issue a GitHub credential, clone the repo, and rewrite the session prompt
/// with repo context. Returns (prepared repo, "owner/name", current token).
async fn prepare_github(
    client: &ViseClient,
    session: &mut Session,
    workdir: &std::path::Path,
) -> anyhow::Result<(github::PreparedRepo, String, String)> {
    let repo = session
        .environment
        .repo
        .clone()
        .ok_or_else(|| anyhow::anyhow!("github_repo session has no environment.repo"))?;

    let credential = client
        .issue_credential(
            &session.id,
            &IssueCredentialRequest {
                provider: "github".to_string(),
            },
        )
        .await?
        .into_inner();
    let secret = credential.secret;

    // Credential-free URL: the clone authenticates via the credential helper,
    // which reads the token file. The token never appears in argv or errors.
    let clone_url = format!("https://github.com/{repo}.git");
    let prepared = github::prepare_workspace(
        workdir,
        &clone_url,
        session.environment.base_branch.as_deref(),
        &secret,
    )
    .await?;

    session.input = format!(
        "You are working in a clone of {repo} (currently on branch {base}). Complete the task \
         below. When done: create a descriptively named branch, commit your work with clear \
         messages, push it, and open a pull request with `gh pr create`. Report the PR URL in \
         your final message.\n\n{input}",
        base = prepared.base_branch,
        input = session.input,
    );

    Ok((prepared, repo, secret))
}

/// Re-issues the GitHub credential periodically and atomically rewrites the
/// token file the clone's credential helper reads. Aborted with the heartbeat.
async fn refresh_token_loop(
    client: ViseClient,
    session_id: String,
    token_file: std::path::PathBuf,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(45 * 60));
    interval.tick().await; // skip the immediate first tick; the token is fresh

    loop {
        interval.tick().await;

        match client
            .issue_credential(
                &session_id,
                &IssueCredentialRequest {
                    provider: "github".to_string(),
                },
            )
            .await
        {
            Ok(response) => {
                let secret = response.into_inner().secret;
                if let Err(error) = github::write_token_file(&token_file, &secret) {
                    tracing::warn!(%session_id, %error, "token file rewrite failed");
                }
            }

            Err(error) => {
                tracing::warn!(%session_id, %error, "credential refresh failed");
            }
        }
    }
}

async fn heartbeat_loop(client: ViseClient, session_id: String, cancel: CancellationToken) {
    let mut interval = tokio::time::interval(Duration::from_secs(20));
    interval.tick().await;

    loop {
        interval.tick().await;

        match client.heartbeat_session(&session_id).await {
            Ok(response) => {
                if response.into_inner().cancel_requested {
                    tracing::info!(%session_id, "cancel requested");
                    cancel.cancel();
                }
            }

            Err(error) => {
                // 409: we no longer hold the session (lease expired / finished elsewhere)
                if error.status().is_some_and(|s| s.is_client_error()) {
                    tracing::warn!(%session_id, %error, "lost session lease");
                    cancel.cancel();
                    return;
                }

                tracing::warn!(%session_id, %error, "heartbeat failed");
            }
        }
    }
}

async fn upload_events(
    client: ViseClient,
    session_id: String,
    mut events: mpsc::Receiver<serde_json::Value>,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(Duration::from_millis(500));
    let mut seq: i64 = 0;
    let mut buffer: Vec<NewSessionEvent> = Vec::new();
    let mut closed = false;

    loop {
        tokio::select! {
            message = events.recv(), if !closed => {
                match message {
                    Some(payload) => {
                        seq += 1;
                        buffer.push(NewSessionEvent { seq, payload });
                    }
                    None => closed = true,
                }
            }

            _ = interval.tick() => {
                while let Ok(payload) = events.try_recv() {
                    seq += 1;
                    buffer.push(NewSessionEvent { seq, payload });
                }

                if !buffer.is_empty() {
                    match client
                        .report_session_events(
                            &session_id,
                            &ReportEventsRequest { events: buffer.clone() },
                        )
                        .await
                    {
                        Ok(_) => buffer.clear(),

                        Err(error) => {
                            // 409: session is no longer ours; the events have nowhere to go
                            if error.status().is_some_and(|s| s.is_client_error()) {
                                anyhow::bail!("dropping {} events: {error}", buffer.len());
                            }

                            tracing::warn!(%session_id, %error, "event upload failed, will retry");
                        }
                    }
                }

                if closed && buffer.is_empty() {
                    return Ok(());
                }
            }
        }
    }
}
