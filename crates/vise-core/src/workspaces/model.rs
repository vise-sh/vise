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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Free-form per-workspace configuration. The core reads nothing from it
    /// yet; it is a home for hosting-layer settings.
    pub settings: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_id_matches_the_seeded_row() {
        assert_eq!(WorkspaceId::DEFAULT.as_str(), "default");
        assert_eq!(WorkspaceId::new("default"), WorkspaceId::DEFAULT);
    }

    #[test]
    fn serializes_as_a_plain_string() {
        let id = WorkspaceId::new("ws_acme");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"ws_acme\"");
        let back: WorkspaceId = serde_json::from_str("\"ws_acme\"").unwrap();
        assert_eq!(back, id);
    }
}
