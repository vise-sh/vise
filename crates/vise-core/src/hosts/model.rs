use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::workspaces::model::WorkspaceId;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Host {
    pub id: String,
    /// The workspace this host serves. It only ever claims sessions from
    /// the same workspace.
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Hosts created by exchanging an enrollment token are ephemeral: the
    /// server deletes them once they stop heartbeating. Hosts enrolled by
    /// hand are not.
    pub ephemeral: bool,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
