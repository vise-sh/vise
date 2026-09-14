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
    /// Read-only GitHub client for PR tracking and follow-up composition;
    /// `None` when neither the GitHub App nor a PAT is configured.
    pub github: Option<crate::github::GitHubApi>,
}
