use std::borrow::Cow;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Identifies the workspace a host or session belongs to.
// Deliberately not `Default`: the default workspace must be named explicitly
// (`WorkspaceId::DEFAULT`) so a caller cannot fall into the shared tenant by
// omission.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
#[schema(value_type = String)]
pub struct WorkspaceId(Cow<'static, str>);

impl WorkspaceId {
    /// The workspace seeded by the migrations. The single-tenant server puts
    /// every host and session here.
    pub const DEFAULT: WorkspaceId = WorkspaceId(Cow::Borrowed("default"));

    pub fn new(id: impl Into<String>) -> Self {
        WorkspaceId(Cow::Owned(id.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WorkspaceId({:?})", self.0)
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for WorkspaceId {
    fn from(id: String) -> Self {
        WorkspaceId::new(id)
    }
}

impl From<&str> for WorkspaceId {
    fn from(id: &str) -> Self {
        WorkspaceId::new(id)
    }
}

impl AsRef<str> for WorkspaceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// How much of a session's event stream may leave the host machine.
///
/// Stored per workspace in `settings.event_fidelity`; hosts learn the policy
/// from the claim response and apply it before events are uploaded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventFidelity {
    /// Events are reported verbatim (the default).
    #[default]
    Full,
    /// The host strips every content string (message text, diffs, file
    /// contents, command output, plan text) before events leave the machine,
    /// keeping only event kind and structure.
    Redacted,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Free-form per-workspace configuration. The core reads
    /// `event_fidelity` from it (see [`Workspace::event_fidelity`]); the rest
    /// is a home for hosting-layer settings.
    pub settings: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

impl Workspace {
    /// The workspace's event-fidelity policy, read from
    /// `settings.event_fidelity`. Absent or unrecognized values mean
    /// [`EventFidelity::Full`], matching the behavior before the setting
    /// existed.
    pub fn event_fidelity(&self) -> EventFidelity {
        self.settings
            .get("event_fidelity")
            .and_then(|value| serde_json::from_value(value.clone()).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_id_matches_the_seeded_row() {
        assert_eq!(WorkspaceId::DEFAULT.as_str(), "default");
        assert_eq!(WorkspaceId::new("default"), WorkspaceId::DEFAULT);
    }

    fn workspace_with_settings(settings: serde_json::Value) -> Workspace {
        Workspace {
            id: WorkspaceId::DEFAULT,
            name: "default".into(),
            settings,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn event_fidelity_defaults_to_full_when_absent() {
        let workspace = workspace_with_settings(serde_json::json!({}));
        assert_eq!(workspace.event_fidelity(), EventFidelity::Full);
    }

    #[test]
    fn event_fidelity_reads_redacted_from_settings() {
        let workspace =
            workspace_with_settings(serde_json::json!({ "event_fidelity": "redacted" }));
        assert_eq!(workspace.event_fidelity(), EventFidelity::Redacted);
    }

    #[test]
    fn event_fidelity_reads_full_from_settings() {
        let workspace = workspace_with_settings(serde_json::json!({ "event_fidelity": "full" }));
        assert_eq!(workspace.event_fidelity(), EventFidelity::Full);
    }

    #[test]
    fn event_fidelity_ignores_unrecognized_values() {
        for bogus in [
            serde_json::json!({ "event_fidelity": "partial" }),
            serde_json::json!({ "event_fidelity": 3 }),
            serde_json::json!({ "event_fidelity": null }),
        ] {
            let workspace = workspace_with_settings(bogus);
            assert_eq!(workspace.event_fidelity(), EventFidelity::Full);
        }
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let id = WorkspaceId::new("ws_acme");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"ws_acme\"");
        let back: WorkspaceId = serde_json::from_str("\"ws_acme\"").unwrap();
        assert_eq!(back, id);
    }
}
