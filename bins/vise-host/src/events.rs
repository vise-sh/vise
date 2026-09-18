//! Event upload pipeline: batches runtime events and reports them to the
//! server. This is the single seam events pass through before leaving the
//! machine, so the workspace's event-fidelity policy is applied here — under
//! [`EventFidelity::Redacted`] every payload is scrubbed with
//! [`crate::redact::redact_payload`] before it is even buffered.

use std::time::Duration;

use tokio::sync::mpsc;
use vise_client::{
    Client as ViseClient,
    types::{EventFidelity, NewSessionEvent, ReportEventsRequest},
};

use crate::redact;

fn enqueue(
    buffer: &mut Vec<NewSessionEvent>,
    seq: &mut i64,
    fidelity: EventFidelity,
    mut payload: serde_json::Value,
) {
    if fidelity == EventFidelity::Redacted {
        redact::redact_payload(&mut payload);
    }
    *seq += 1;
    buffer.push(NewSessionEvent { seq: *seq, payload });
}

pub async fn upload_events(
    client: ViseClient,
    session_id: String,
    fidelity: EventFidelity,
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
                    Some(payload) => enqueue(&mut buffer, &mut seq, fidelity, payload),
                    None => closed = true,
                }
            }

            _ = interval.tick() => {
                while let Ok(payload) = events.try_recv() {
                    enqueue(&mut buffer, &mut seq, fidelity, payload);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{EchoRuntime, SessionRuntime};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;
    use vise_client::types::{Agent, Environment, Session, SessionStatus, WorkspaceId};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SESSION_ID: &str = "ses_test";

    fn session(input: &str) -> Session {
        let now = chrono::Utc::now();
        Session {
            id: SESSION_ID.to_string(),
            workspace_id: WorkspaceId("default".to_string()),
            agent: Agent {
                harness: "echo".to_string(),
                model: "test".to_string(),
                instructions: String::new(),
                mcp_servers: vec![],
            },
            environment: Environment {
                kind: "self_hosted".to_string(),
                repo: None,
                base_branch: None,
            },
            input: input.to_string(),
            status: SessionStatus::Running,
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
        }
    }

    async fn events_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(format!("/hosts/sessions/{SESSION_ID}/events")))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        server
    }

    /// Every event payload the mock server received, in order, across all
    /// report_session_events request bodies.
    async fn received_payloads(server: &MockServer) -> Vec<serde_json::Value> {
        let mut payloads = Vec::new();
        for request in server.received_requests().await.unwrap() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            for event in body["events"].as_array().unwrap() {
                payloads.push(event["payload"].clone());
            }
        }
        payloads
    }

    async fn raw_bodies(server: &MockServer) -> String {
        let mut all = String::new();
        for request in server.received_requests().await.unwrap() {
            all.push_str(&String::from_utf8_lossy(&request.body));
        }
        all
    }

    #[tokio::test]
    async fn full_fidelity_uploads_payloads_verbatim() {
        let server = events_server().await;
        let client = ViseClient::new(&server.uri());
        let (tx, rx) = mpsc::channel(16);

        let sent = vec![
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "the secret plan" }
            }),
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Run tests",
                "rawInput": { "command": "cargo test" }
            }),
        ];
        for payload in &sent {
            tx.send(payload.clone()).await.unwrap();
        }
        drop(tx);

        upload_events(client, SESSION_ID.to_string(), EventFidelity::Full, rx)
            .await
            .unwrap();

        // Byte-identical default path: what went in is exactly what went out.
        assert_eq!(received_payloads(&server).await, sent);
    }

    #[tokio::test]
    async fn redacted_fidelity_never_puts_content_in_the_request_body() {
        let server = events_server().await;
        let client = ViseClient::new(&server.uri());
        let (tx, rx) = mpsc::channel(16);

        tx.send(json!({
            "sessionId": "acp-1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "SECRET_MESSAGE_TEXT" }
            }
        }))
        .await
        .unwrap();
        tx.send(json!({
            "sessionId": "acp-1",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_1",
                "status": "completed",
                "content": [{
                    "type": "diff",
                    "path": "/repo/src/lib.rs",
                    "oldText": "SECRET_OLD_CODE",
                    "newText": "SECRET_NEW_CODE"
                }],
                "rawOutput": { "stdout": "SECRET_COMMAND_OUTPUT" }
            }
        }))
        .await
        .unwrap();
        drop(tx);

        upload_events(client, SESSION_ID.to_string(), EventFidelity::Redacted, rx)
            .await
            .unwrap();

        let bodies = raw_bodies(&server).await;
        assert!(!bodies.is_empty());
        assert!(!bodies.contains("SECRET"), "content leaked: {bodies}");

        // Structure survives: kinds, tool-call ids/statuses, paths, seqs.
        let payloads = received_payloads(&server).await;
        assert_eq!(payloads.len(), 2);
        assert_eq!(
            payloads[0]["update"]["sessionUpdate"],
            "agent_message_chunk"
        );
        assert_eq!(payloads[1]["update"]["toolCallId"], "call_1");
        assert_eq!(payloads[1]["update"]["status"], "completed");
        assert_eq!(
            payloads[1]["update"]["content"][0]["path"],
            "/repo/src/lib.rs"
        );
    }

    /// End-to-end through the host layer with the echo harness: the runtime's
    /// own events flow through the real upload pipeline, and nothing of the
    /// session input survives in the outbound request bodies.
    #[tokio::test]
    async fn echo_harness_events_are_redacted_end_to_end() {
        let server = events_server().await;
        let client = ViseClient::new(&server.uri());
        let (tx, rx) = mpsc::channel(16);

        let session = session("SECRET_TASK_INPUT do the thing");
        let runtime = EchoRuntime;
        let workspace = std::env::temp_dir();
        let cancel = CancellationToken::new();

        let uploader = tokio::spawn(upload_events(
            client,
            SESSION_ID.to_string(),
            EventFidelity::Redacted,
            rx,
        ));

        runtime
            .run(&session, &workspace, None, tx, cancel)
            .await
            .unwrap();
        uploader.await.unwrap().unwrap();

        let bodies = raw_bodies(&server).await;
        assert!(!bodies.contains("SECRET_TASK_INPUT"), "input leaked");
        assert!(!bodies.contains("Echo"), "echoed content leaked");

        // The echo runtime emits three chunks; kind, structure and sequence
        // numbers all survive redaction.
        let mut seqs = Vec::new();
        for request in server.received_requests().await.unwrap() {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            for event in body["events"].as_array().unwrap() {
                assert_eq!(event["payload"]["sessionUpdate"], "agent_message_chunk");
                assert_eq!(event["payload"]["content"]["type"], "text");
                seqs.push(event["seq"].as_i64().unwrap());
            }
        }
        assert_eq!(seqs, vec![1, 2, 3]);
    }
}
