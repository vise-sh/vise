use std::collections::HashMap;
use std::sync::Arc;

use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};
use vise_core::workspaces::model::WorkspaceId;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionService<PostgresSessionRepository>>,
    pub hosts: Arc<HostService<PostgresHostRepository>>,
    /// The workspace every user-facing request is scoped to. This server is
    /// single-tenant, so `vise-server` sets it to [`WorkspaceId::DEFAULT`];
    /// host-driven routes derive their scope from the authenticated host
    /// instead.
    pub workspace: WorkspaceId,
    /// Credential providers by name ("github", ...). Empty = none configured.
    pub credentials: HashMap<String, Arc<dyn crate::credentials::CredentialProvider>>,
    /// Read-only GitHub client for PR tracking and follow-up composition;
    /// `None` when neither the GitHub App nor a PAT is configured.
    pub github: Option<crate::github::GitHubApi>,
}
