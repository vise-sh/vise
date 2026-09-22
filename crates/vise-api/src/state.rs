use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::FromRef;
use vise_core::enrollment::{
    postgres::PostgresEnrollmentTokenRepository, service::EnrollmentTokenService,
};
use vise_core::hosts::{postgres::PostgresHostRepository, service::HostService};
use vise_core::sessions::{postgres::PostgresSessionRepository, service::SessionService};
use vise_core::workspaces::model::WorkspaceId;
use vise_core::workspaces::postgres::PostgresWorkspaceRepository;

use crate::auth::CallerExtractor;

#[derive(Clone)]
pub struct AppState {
    pub sessions: Arc<SessionService<PostgresSessionRepository>>,
    /// Cron-scheduled session spawns. A background scheduler ticks these; the
    /// user-facing routes CRUD them and drive off-schedule runs.
    pub routines: Arc<
        vise_core::routines::service::RoutineService<
            vise_core::routines::postgres::PostgresRoutineRepository,
            vise_core::sessions::postgres::PostgresSessionRepository,
        >,
    >,
    pub hosts: Arc<HostService<PostgresHostRepository>>,
    /// Workspace rows, read for per-workspace policy (e.g. event fidelity)
    /// when a host claims work.
    pub workspaces: Arc<PostgresWorkspaceRepository>,
    /// The workspace every user-facing request is scoped to. This server is
    /// single-tenant, so `vise-server` sets it to [`WorkspaceId::DEFAULT`];
    /// host-driven routes derive their scope from the authenticated host
    /// instead.
    pub workspace: WorkspaceId,
    /// Reusable enrollment tokens and their exchange for ephemeral hosts.
    pub enrollment:
        Arc<EnrollmentTokenService<PostgresEnrollmentTokenRepository, PostgresHostRepository>>,
    /// Credential providers by name ("github", ...). Empty = none configured.
    pub credentials: HashMap<String, Arc<dyn crate::credentials::CredentialProvider>>,
    /// Read-only GitHub client for PR tracking and follow-up composition;
    /// `None` when neither the GitHub App nor a PAT is configured.
    pub github: Option<crate::github::GitHubApi>,
    /// Resolves the caller of user-facing routes (sessions, host enrollment),
    /// including the workspace they are scoped to. `vise-server` uses
    /// [`crate::auth::from_env`], whose extractors put every caller in the
    /// default workspace; an external composition can supply its own
    /// [`CallerExtractor`]. Host-driven routes derive their scope from the
    /// authenticated host instead.
    pub caller: Arc<dyn CallerExtractor>,
}

impl FromRef<AppState> for Arc<dyn CallerExtractor> {
    fn from_ref(state: &AppState) -> Self {
        state.caller.clone()
    }
}
