use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::sessions::model::{Agent, Environment};
use crate::workspaces::model::WorkspaceId;

/// The inline session spec a routine instantiates on each fire. Mirrors the
/// public `CreateSessionRequest` shape exactly (agent + environment + input),
/// so when sibling primitives (e.g. deliverable policy) add fields to session
/// creation, routines inherit them without the routine layer knowing.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionSpec {
    pub agent: Agent,
    pub environment: Environment,
    pub input: String,
}

/// An API-managed, cron-scheduled session spawn. A background scheduler
/// spawns one session per due routine, in the routine's own workspace.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Routine {
    pub id: String,
    pub workspace_id: WorkspaceId,
    pub name: String,
    /// Standard 5-field cron expression.
    pub cron: String,
    /// IANA timezone name (e.g. "America/New_York") the cron is interpreted in.
    pub timezone: String,
    pub spec: SessionSpec,
    /// A disabled routine is never fired; it stays configured so it can be
    /// resumed without recreating it.
    pub enabled: bool,
    /// The next UTC instant this routine is due. The scheduler's claim key.
    pub next_run_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
