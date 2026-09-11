use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionNotification, StopReason, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent, Client, ConnectionTo, Error};
use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vise_client::types::Session;

use crate::runtime::{RunOutcome, SessionRuntime};

/// Runs a session by spawning an ACP-capable agent as a child process and
/// driving one prompt turn over stdio.
pub struct AcpProcessRuntime;

// The harness → command map is host-side config by design (the server never
// resolves commands). Hardcoded until the host grows a config file.
//
// The returned string may embed `gh_token` and must never be logged.
fn resolve_command(harness: &str, gh_token: Option<&str>) -> anyhow::Result<String> {
    let base = match harness {
        "claude-code" => "npx -y @agentclientprotocol/claude-agent-acp@latest",
        other => anyhow::bail!("unknown harness: {other}"),
    };

    Ok(match gh_token {
        Some(token) => format!("env GH_TOKEN={token} {base}"),
        None => base.to_string(),
    })
}

/// Strip the credential from error text before it reaches anyhow/tracing;
/// the agent command (and the child's stderr) can embed the GH token.
fn redact(text: String, secret: Option<&str>) -> String {
    match secret {
        Some(secret) if !secret.is_empty() => text.replace(secret, "[redacted]"),
        _ => text,
    }
}

fn internal_error(message: impl ToString) -> Error {
    Error::internal_error().data(message.to_string())
}

fn stop_reason_str(stop_reason: StopReason) -> String {
    match serde_json::to_value(stop_reason) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{stop_reason:?}"),
    }
}

#[async_trait]
impl SessionRuntime for AcpProcessRuntime {
    async fn run(
        &self,
        session: &Session,
        workspace: &Path,
        gh_token: Option<String>,
        events: mpsc::Sender<serde_json::Value>,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        let command = resolve_command(&session.agent.harness, gh_token.as_deref())?;
        // The command may embed a credential; never include it in errors or logs.
        let agent = AcpAgent::from_str(&command).map_err(|e| {
            let msg = redact(format!("{e:?}"), gh_token.as_deref());
            anyhow::anyhow!(
                "invalid agent command for harness {}: {msg}",
                session.agent.harness
            )
        })?;

        // The foreground closure can only return Result<(), Error>, so the
        // outcome travels out through this slot.
        let outcome: Arc<Mutex<Option<RunOutcome>>> = Arc::new(Mutex::new(None));

        let notification_events = events.clone();
        let permission_events = events.clone();

        let text = if session.agent.instructions.is_empty() {
            session.input.clone()
        } else {
            format!("{}\n\n{}", session.agent.instructions, session.input)
        };
        let workspace = workspace.to_path_buf();
        let foreground_outcome = outcome.clone();

        let result = Client
            .builder()
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    // Stored verbatim: ACP does the event modeling, vise doesn't.
                    let payload =
                        serde_json::to_value(&notification).map_err(internal_error)?;

                    notification_events
                        .send(payload)
                        .await
                        .map_err(internal_error)?;

                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    // v1 policy: auto-approve everything, recording the request
                    // and the auto-response as events for an audit trail.
                    let option_id = request.options.first().map(|opt| opt.option_id.clone());

                    let _ = permission_events
                        .send(json!({
                            "permissionRequest": serde_json::to_value(&request).ok(),
                            "autoApproved": serde_json::to_value(&option_id).ok(),
                        }))
                        .await;

                    if let Some(id) = option_id {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                        ))
                    } else {
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Cancelled,
                        ))
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;

                let new_session = connection
                    .send_request(NewSessionRequest::new(workspace))
                    .block_task()
                    .await?;

                let session_id = new_session.session_id;

                let prompt = connection
                    .send_request(PromptRequest::new(
                        session_id.clone(),
                        vec![ContentBlock::Text(TextContent::new(text))],
                    ))
                    .block_task();
                tokio::pin!(prompt);

                let response = tokio::select! {
                    response = &mut prompt => response?,

                    _ = cancel.cancelled() => {
                        let _ = connection
                            .send_notification(CancelNotification::new(session_id.clone()));

                        // Grace period for the agent to acknowledge; dropping the
                        // connection afterwards kills the child process group.
                        match tokio::time::timeout(Duration::from_secs(10), &mut prompt).await {
                            Ok(Ok(response)) => response,
                            _ => {
                                *foreground_outcome.lock().unwrap() = Some(RunOutcome::Cancelled);
                                return Ok(());
                            }
                        }
                    }
                };

                *foreground_outcome.lock().unwrap() = Some(match response.stop_reason {
                    StopReason::Cancelled => RunOutcome::Cancelled,
                    stop_reason => RunOutcome::Completed {
                        stop_reason: stop_reason_str(stop_reason),
                    },
                });

                Ok(())
            })
            .await;

        // AcpAgent folds the child's exit status and stderr tail into the error.
        if let Err(error) = result {
            let msg = redact(format!("{error:?}"), gh_token.as_deref());
            let outcome = outcome.lock().unwrap().take();
            if let Some(outcome) = outcome {
                tracing::warn!(error = %msg, "acp connection error after turn finished");
                return Ok(outcome);
            }
            anyhow::bail!("acp agent failed: {msg}");
        }

        outcome
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("acp connection closed before the prompt turn finished"))
    }
}
