use std::collections::HashMap;
use std::sync::Arc;

use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionService<PostgresSessionRepository>>,
    pub hosts: Arc<HostService<PostgresHostRepository>>,
    /// Credential providers by name ("github", ...). Empty = none configured.
    pub credentials: HashMap<String, Arc<dyn crate::credentials::CredentialProvider>>,
    /// Read-only GitHub access for PR tracking and follow-up composition.
    pub github: crate::github::GitHubReadClient,
    /// Whether this server runs the PR poller. When false, `--watch` on a
    /// completed session ends immediately instead of waiting for PR events
    /// that would never arrive.
    pub pr_tracking_enabled: bool,
}

impl AppState {
    pub fn github_credentials(&self) -> Option<&Arc<dyn crate::credentials::CredentialProvider>> {
        self.credentials.get("github")
    }
}
