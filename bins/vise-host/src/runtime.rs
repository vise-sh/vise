use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use vise_client::types::Session;

#[derive(Debug)]
pub enum RunOutcome {
    Completed { stop_reason: String },
    Cancelled,
}

/// The "CRI" seam: one runtime contract, many harness/environment implementations.
#[async_trait]
pub trait SessionRuntime: Send + Sync {
    async fn run(
        &self,
        session: &Session,
        workspace: &Path,
        gh_token: Option<String>,
        events: mpsc::Sender<serde_json::Value>,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome>;
}

/// Fake runtime that echoes the session input back as ACP-style
/// agent_message_chunk events. Proves the control loop before ACP lands.
pub struct EchoRuntime;

#[async_trait]
impl SessionRuntime for EchoRuntime {
    async fn run(
        &self,
        session: &Session,
        _workspace: &Path,
        _gh_token: Option<String>,
        events: mpsc::Sender<serde_json::Value>,
        cancel: CancellationToken,
    ) -> anyhow::Result<RunOutcome> {
        let chunks = ["Echo: ", session.input.as_str(), "\n"];

        for text in chunks {
            events
                .send(json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": text }
                }))
                .await?;

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = cancel.cancelled() => return Ok(RunOutcome::Cancelled),
            }
        }

        Ok(RunOutcome::Completed {
            stop_reason: "end_turn".to_string(),
        })
    }
}
