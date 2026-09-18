//! Event-fidelity redaction. When a workspace's policy is `redacted`, every
//! event payload is passed through [`redact_payload`] before it leaves the
//! machine: the event's kind and structure survive (tool-call names, statuses,
//! file paths, plan-step counts, sequence numbers), but every content string —
//! message text, diffs, file contents, command output, plan text — is
//! replaced with a fixed marker.

use serde_json::Value;

/// What redacted string values are replaced with.
pub const REDACTED: &str = "[redacted]";

/// String values under these keys are structural metadata, not content, and
/// are kept verbatim. Everything else that is a string gets replaced.
const KEEP_KEYS: &[&str] = &[
    // event identity / kind
    "sessionUpdate",
    "sessionId",
    "type",
    "kind",
    "status",
    "stopReason",
    // tool calls
    "toolCallId",
    "name",
    "terminalId",
    // files
    "path",
    "uri",
    "mimeType",
    // plans
    "priority",
    // permission requests (option ids, not content)
    "optionId",
    "autoApproved",
    // session modes
    "modeId",
    "currentModeId",
];

/// Redact one event payload in place. Pure over the JSON value: keys,
/// nesting, array lengths, numbers, booleans and nulls are untouched; string
/// values are replaced with [`REDACTED`] unless their key marks them as
/// structural metadata.
///
/// `title` is special-cased: inside a tool call (an object that carries a
/// `toolCallId`) it is the tool-call name and is kept; anywhere else (e.g. a
/// session-info title derived from the conversation) it is content.
pub fn redact_payload(payload: &mut Value) {
    match payload {
        Value::Object(map) => {
            let is_tool_call = map.contains_key("toolCallId");
            for (key, value) in map.iter_mut() {
                match value {
                    Value::String(text) => {
                        let keep =
                            KEEP_KEYS.contains(&key.as_str()) || (key == "title" && is_tool_call);
                        if !keep {
                            *text = REDACTED.to_string();
                        }
                    }
                    _ => redact_payload(value),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_payload(item);
            }
        }
        // Strings not reachable through an object key have no structural
        // meaning we can vouch for; scalar payloads other than strings carry
        // no content.
        Value::String(text) => *text = REDACTED.to_string(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn redacted(mut payload: Value) -> Value {
        redact_payload(&mut payload);
        payload
    }

    #[test]
    fn strips_message_text_but_keeps_kind_and_structure() {
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "the secret plan" }
            }
        }));

        assert_eq!(out["sessionId"], "sess-1");
        assert_eq!(out["update"]["sessionUpdate"], "agent_message_chunk");
        assert_eq!(out["update"]["content"]["type"], "text");
        assert_eq!(out["update"]["content"]["text"], REDACTED);
        assert!(!out.to_string().contains("secret"));
    }

    #[test]
    fn strips_echo_runtime_chunks_without_the_notification_wrapper() {
        let out = redacted(json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "Echo: do the thing" }
        }));

        assert_eq!(out["sessionUpdate"], "agent_message_chunk");
        assert_eq!(out["content"]["text"], REDACTED);
        assert!(!out.to_string().contains("Echo"));
    }

    #[test]
    fn keeps_tool_call_name_kind_status_and_paths() {
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "call_1",
                "title": "Read file",
                "kind": "read",
                "status": "in_progress",
                "locations": [{ "path": "/repo/src/lib.rs", "line": 42 }],
                "rawInput": { "file_path": "top secret argument", "offset": 10 }
            }
        }));

        let update = &out["update"];
        assert_eq!(update["toolCallId"], "call_1");
        assert_eq!(update["title"], "Read file");
        assert_eq!(update["kind"], "read");
        assert_eq!(update["status"], "in_progress");
        assert_eq!(update["locations"][0]["path"], "/repo/src/lib.rs");
        assert_eq!(update["locations"][0]["line"], 42);
        // rawInput strings are content; numbers are not.
        assert_eq!(update["rawInput"]["file_path"], REDACTED);
        assert_eq!(update["rawInput"]["offset"], 10);
    }

    #[test]
    fn strips_diffs_file_contents_and_command_output() {
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call_2",
                "status": "completed",
                "content": [
                    {
                        "type": "diff",
                        "path": "/repo/src/main.rs",
                        "oldText": "fn old() {}",
                        "newText": "fn new() {}"
                    },
                    {
                        "type": "content",
                        "content": { "type": "text", "text": "cargo test output..." }
                    }
                ],
                "rawOutput": { "stdout": "compiled 3 crates", "exit_code": 0 }
            }
        }));

        let content = &out["update"]["content"];
        assert_eq!(content[0]["type"], "diff");
        assert_eq!(content[0]["path"], "/repo/src/main.rs");
        assert_eq!(content[0]["oldText"], REDACTED);
        assert_eq!(content[0]["newText"], REDACTED);
        assert_eq!(content[1]["content"]["text"], REDACTED);
        assert_eq!(out["update"]["rawOutput"]["stdout"], REDACTED);
        assert_eq!(out["update"]["rawOutput"]["exit_code"], 0);

        let text = out.to_string();
        assert!(!text.contains("fn old"));
        assert!(!text.contains("fn new"));
        assert!(!text.contains("cargo test output"));
        assert!(!text.contains("compiled"));
    }

    #[test]
    fn strips_plan_text_but_keeps_step_count_priority_and_status() {
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "plan",
                "entries": [
                    { "content": "step one: read the config", "priority": "high", "status": "pending" },
                    { "content": "step two: rewrite it", "priority": "low", "status": "pending" }
                ]
            }
        }));

        let entries = out["update"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "plan-step count must survive");
        for entry in entries {
            assert_eq!(entry["content"], REDACTED);
        }
        assert_eq!(entries[0]["priority"], "high");
        assert_eq!(entries[1]["status"], "pending");
        assert!(!out.to_string().contains("step one"));
    }

    #[test]
    fn strips_session_title_outside_tool_calls() {
        // SessionInfoUpdate titles are derived from the conversation: content.
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "session_info_update",
                "title": "Fixing the flaky login test"
            }
        }));

        assert_eq!(out["update"]["title"], REDACTED);
    }

    #[test]
    fn strips_thought_chunks() {
        let out = redacted(json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking about secrets" }
            }
        }));

        assert_eq!(out["update"]["content"]["text"], REDACTED);
    }

    #[test]
    fn redacts_permission_request_events_but_keeps_option_ids() {
        let out = redacted(json!({
            "permissionRequest": {
                "sessionId": "sess-1",
                "toolCall": {
                    "toolCallId": "call_3",
                    "title": "Run tests",
                    "kind": "execute",
                    "rawInput": { "command": "rm -rf /tmp/scratch" }
                },
                "options": [
                    { "optionId": "allow", "name": "Allow", "kind": "allow_once" }
                ]
            },
            "autoApproved": "allow"
        }));

        let request = &out["permissionRequest"];
        assert_eq!(request["toolCall"]["title"], "Run tests");
        assert_eq!(request["toolCall"]["rawInput"]["command"], REDACTED);
        assert_eq!(request["options"][0]["optionId"], "allow");
        assert_eq!(out["autoApproved"], "allow");
        assert!(!out.to_string().contains("rm -rf"));
    }

    #[test]
    fn keeps_numbers_booleans_and_nulls() {
        let payload = json!({
            "sessionUpdate": "usage_update",
            "usedTokens": 1234,
            "cached": true,
            "cost": null,
            "at": 1758000000
        });
        assert_eq!(redacted(payload.clone()), payload);
    }

    #[test]
    fn redacts_bare_string_payloads() {
        assert_eq!(redacted(json!("free-floating content")), json!(REDACTED));
    }
}
