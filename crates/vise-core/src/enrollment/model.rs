use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::workspaces::model::WorkspaceId;

/// A reusable enrollment token. Carries no secret: only the SHA-256 hash of
/// the `venroll_` secret is stored, and the secret itself is returned once,
/// at mint.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnrollmentToken {
    pub id: String,
    /// The workspace every host enrolled through this token lands in.
    pub workspace_id: WorkspaceId,
    /// Maximum number of exchanges; `None` = unlimited.
    pub max_uses: Option<i64>,
    /// How many exchanges have succeeded so far.
    pub uses: i64,
    /// Set when the token was revoked; a revoked token can no longer be
    /// exchanged.
    pub revoked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}
