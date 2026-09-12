use std::io::Write;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use serde_json::Value;
use vise_client::{
    Client as ViseClient,
    types::{Agent, CreateSessionRequest, Environment, FollowUpRequest, Session},
};

#[derive(Parser)]
#[command(name = "vise")]
#[command(version)]
#[command(about = "CLI for Vise")]
struct Cli {
    /// URL of the Vise API
    #[arg(short, long, default_value = "http://localhost:3000")]
    url: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Manage sessions
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },

    /// Manage hosts
    Hosts {
        #[command(subcommand)]
        command: HostsCommand,
    },
}

#[derive(Subcommand)]
enum HostsCommand {
    /// List enrolled hosts
    Ls,

    /// Enroll a new host and print its token (shown exactly once)
    Create {
        /// Host name
        name: String,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List sessions
    Ls,

    /// Get a session
    Get {
        /// Session ID
        session: String,
    },

    /// Create a session
    Create {
        /// The prompt to run
        input: String,

        /// Agent harness ("claude-code", or "echo" for the fake runtime)
        #[arg(long, default_value = "claude-code")]
        harness: String,

        /// Model hint passed to the harness
        #[arg(long, default_value = "")]
        model: String,

        /// System-prompt-ish guidance prepended to the input
        #[arg(long, default_value = "")]
        instructions: String,

        /// Target GitHub repository ("owner/name"); switches environment to github_repo
        #[arg(long)]
        repo: Option<String>,

        /// Base branch for --repo (default: repo default branch)
        #[arg(long)]
        base_branch: Option<String>,

        /// Watch the session after creating it
        #[arg(long)]
        watch: bool,
    },

    /// Get events for a session
    Events {
        /// Session ID
        session: String,

        /// Only return events after this sequence number
        #[arg(long)]
        after_seq: Option<i64>,
    },

    /// Request cancellation of a session
    Cancel {
        /// Session ID
        session: String,
    },

    /// Live-tail a session's events. On a finished session that opened a PR,
    /// keeps tailing PR review/check state until the PR merges or closes.
    Watch {
        /// Session ID
        session: String,

        /// Replay from this sequence number (0 = full history)
        #[arg(long, default_value_t = 0)]
        after_seq: i64,
    },

    /// Spawn a follow-up session that addresses review feedback on a session's PR
    FollowUp {
        /// Session ID of the session (or an earlier follow-up) whose PR to address
        session: String,

        /// Extra guidance for the agent, appended after the review feedback
        #[arg(long)]
        instructions: Option<String>,

        /// Watch the follow-up after creating it
        #[arg(long)]
        watch: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let client = ViseClient::new(&cli.url);

    match cli.command {
        Command::Sessions { command } => match command {
            SessionsCommand::Ls => {
                let sessions = client.list_sessions().await?.into_inner();
                render_session_table(&sessions.sessions);
            }

            SessionsCommand::Get { session } => {
                let session = client.get_session(&session).await?.into_inner();
                println!("{}", serde_json::to_string_pretty(&session)?);
                render_pr_summary(&session);
            }

            SessionsCommand::Create {
                input,
                harness,
                model,
                instructions,
                repo,
                base_branch,
                watch,
            } => {
                let environment = match &repo {
                    Some(r) => Environment {
                        kind: "github_repo".into(),
                        repo: Some(r.clone()),
                        base_branch: base_branch.clone(),
                    },
                    None => Environment {
                        kind: "self_hosted".into(),
                        repo: None,
                        base_branch: None,
                    },
                };

                let session = client
                    .create_session(&CreateSessionRequest {
                        agent: Agent {
                            harness,
                            model,
                            instructions,
                            mcp_servers: vec![],
                        },
                        environment,
                        input,
                    })
                    .await?
                    .into_inner();

                if watch {
                    eprintln!("created {}", session.id);
                    watch_session(&client, &cli.url, &session.id, 0).await?;
                } else {
                    println!("{}", serde_json::to_string_pretty(&session)?);
                }
            }

            SessionsCommand::Events { session, after_seq } => {
                let events = client
                    .get_events(&session, after_seq, None)
                    .await?
                    .into_inner();
                println!("{}", serde_json::to_string_pretty(&events)?);
            }

            SessionsCommand::Cancel { session } => {
                let session = client.cancel_session(&session).await?.into_inner();
                println!("{}", serde_json::to_string_pretty(&session)?);
            }

            SessionsCommand::Watch { session, after_seq } => {
                watch_session(&client, &cli.url, &session, after_seq).await?;
            }

            SessionsCommand::FollowUp {
                session,
                instructions,
                watch,
            } => {
                let created = client
                    .follow_up_session(
                        &session,
                        &FollowUpRequest {
                            instructions,
                            agent: None,
                        },
                    )
                    .await?
                    .into_inner();

                if watch {
                    eprintln!("created follow-up {} (parent {session})", created.id);
                    watch_session(&client, &cli.url, &created.id, 0).await?;
                } else {
                    println!("{}", serde_json::to_string_pretty(&created)?);
                }
            }
        },

        Command::Hosts { command } => match command {
            HostsCommand::Ls => {
                let hosts = client.list_hosts().await?.into_inner();
                println!("{}", serde_json::to_string_pretty(&hosts)?);
            }

            HostsCommand::Create { name } => {
                let enrolled = client
                    .enroll_host(&vise_client::types::EnrollHostRequest { name })
                    .await?
                    .into_inner();

                println!("{}", serde_json::to_string_pretty(&enrolled.host)?);
                eprintln!("\ntoken (shown once — save it):\n{}", enrolled.token);
                eprintln!(
                    "\nrun the host with:\n  VISE_HOST_TOKEN={} cargo run -p vise-host",
                    enrolled.token
                );
            }
        },
    }

    Ok(())
}

async fn watch_session(
    client: &ViseClient,
    base_url: &str,
    session_id: &str,
    after_seq: i64,
) -> anyhow::Result<()> {
    let url = format!("{base_url}/sessions/{session_id}/events/stream?after_seq={after_seq}");

    let response = reqwest::get(&url).await?.error_for_status()?;
    let mut body = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = body.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));

        // SSE events are separated by a blank line
        while let Some(boundary) = buffer.find("\n\n") {
            let raw = buffer[..boundary].to_string();
            buffer.drain(..boundary + 2);
            render_sse_event(&raw);
        }
    }

    println!();

    let session = client.get_session(session_id).await?.into_inner();
    if let Some(outcome) = &session.outcome {
        match (
            outcome.kind.as_str(),
            outcome.pr_url.as_deref(),
            outcome.branch.as_deref(),
        ) {
            ("pr_opened", Some(url), _) => println!("PR: {url}"),
            ("pr_updated", Some(url), _) => println!("PR updated: {url}"),
            ("pushed_no_pr", _, Some(branch)) => println!("pushed branch {branch} (no PR)"),
            ("uncommitted_changes", _, _) => {
                println!("warning: agent left uncommitted work; workspace kept on host")
            }
            ("no_changes", _, _) => println!("no changes made"),
            _ => {}
        }
    }
    render_pr_summary(&session);
    if let Some(parent) = &session.parent_session_id {
        println!("PR tracking continues on the root session (parent: {parent})");
    }

    Ok(())
}

fn pr_state_columns(session: &Session) -> (String, String) {
    match &session.pr_status {
        Some(status) => (
            status.state.to_string(),
            status
                .checks
                .as_ref()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ),
        None => ("-".to_string(), "-".to_string()),
    }
}

fn render_session_table(sessions: &[Session]) {
    println!(
        "{:<32} {:<10} {:<20} {:<18} {:<8} CREATED",
        "ID", "STATUS", "OUTCOME", "PR", "CHECKS"
    );
    for session in sessions {
        let outcome = session
            .outcome
            .as_ref()
            .map(|o| o.kind.clone())
            .unwrap_or_else(|| "-".to_string());
        let (pr, checks) = pr_state_columns(session);
        println!(
            "{:<32} {:<10} {:<20} {:<18} {:<8} {}",
            session.id,
            session.status,
            outcome,
            pr,
            checks,
            session
                .created_at
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
    }
}

/// One line describing the PR tracking snapshot, when there is one.
fn render_pr_summary(session: &Session) {
    let Some(status) = &session.pr_status else {
        return;
    };
    let url = session
        .outcome
        .as_ref()
        .and_then(|o| o.pr_url.as_deref())
        .unwrap_or("-");
    let (state, checks) = pr_state_columns(session);
    println!(
        "PR {url}: {state}, checks {checks} (synced {})",
        status
            .last_synced_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
}

fn render_sse_event(raw: &str) {
    let mut event_name = "message";
    let mut data = String::new();

    for line in raw.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_name = value.trim();
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.trim_start());
        }
        // ignore id: and keep-alive comment lines
    }

    if event_name == "done" {
        match data.as_str() {
            "merged" | "closed" => println!("\n── pr {data} ──"),
            _ => println!("\n── session {data} ──"),
        }
        return;
    }

    let Ok(event) = serde_json::from_str::<Value>(&data) else {
        return;
    };

    render_payload(&event["payload"]);
}

fn render_payload(payload: &Value) {
    // ACP notifications arrive as { sessionId, update: { sessionUpdate, ... } };
    // echo-runtime and audit events are flat.
    let update = if payload["update"].is_object() {
        &payload["update"]
    } else {
        payload
    };

    if payload["permissionRequest"].is_object() {
        println!("\n[permission auto-approved]");
        return;
    }

    // Server-side PR tracking transitions on finished sessions.
    if let Some(kind) = payload["type"].as_str() {
        let from = payload["from"].as_str().unwrap_or("unknown");
        let to = payload["to"].as_str().unwrap_or("none");
        match kind {
            "pr_state_changed" => println!("\n[pr] {from} -> {to}"),
            "checks_state_changed" => println!("\n[checks] {from} -> {to}"),
            other => println!("\n[{other}]"),
        }
        return;
    }

    match update["sessionUpdate"].as_str() {
        Some("agent_message_chunk") => {
            if let Some(text) = update["content"]["text"].as_str() {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
        }

        Some("tool_call") => {
            let title = update["title"].as_str().unwrap_or("tool call");
            println!("\n[tool] {title}");
        }

        Some("agent_thought_chunk") | Some("tool_call_update") | Some("plan") => {}

        Some(other) => println!("\n[{other}]"),

        None => {}
    }
}
