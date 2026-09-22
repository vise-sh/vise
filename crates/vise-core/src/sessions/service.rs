use chrono::Utc;

use super::{
    model::{Session, SessionOutcome, SessionStatus},
    repository::{PrSync, SessionRepository},
};
use crate::workspaces::model::WorkspaceId;

pub struct SessionService<R> {
    repository: R,
}

/// Longest parent chain we are willing to walk when resolving the root of a
/// follow-up. Chains are short in practice; the bound guards against cycles.
const MAX_FOLLOW_UP_DEPTH: usize = 64;

impl<R> SessionService<R>
where
    R: SessionRepository,
{
    pub fn new(repository: R) -> Self {
        Self { repository }
    }

    pub async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Session>> {
        self.repository.list(workspace).await
    }

    pub async fn get(&self, workspace: &WorkspaceId, id: &str) -> anyhow::Result<Option<Session>> {
        self.repository.get(workspace, id).await
    }

    /// Sessions spawned by a given routine in `workspace`, newest first.
    pub async fn list_by_routine(
        &self,
        workspace: &WorkspaceId,
        routine_id: &str,
    ) -> anyhow::Result<Vec<Session>> {
        self.repository.list_by_routine(workspace, routine_id).await
    }

    /// Queue a new session in `workspace`. A follow-up's `parent_session_id`
    /// must name a session in the same workspace; callers resolve the parent
    /// with [`get`](Self::get) first, which cannot cross workspaces.
    pub async fn create(
        &self,
        workspace: WorkspaceId,
        agent: super::model::Agent,
        environment: super::model::Environment,
        input: String,
        parent_session_id: Option<String>,
        routine_id: Option<String>,
    ) -> anyhow::Result<Session> {
        let now = Utc::now();

        let session = Session {
            id: crate::id::new_id("ses"),
            workspace_id: workspace,
            agent,
            environment,
            input,
            status: SessionStatus::Pending,
            host_id: None,
            lease_expires_at: None,
            started_at: None,
            finished_at: None,
            stop_reason: None,
            error: None,
            outcome: None,
            pr_status: None,
            parent_session_id,
            routine_id,
            cancel_requested: false,
            created_at: now,
            updated_at: now,
        };

        self.repository.create(session).await
    }

    /// Walk the `parent_session_id` chain to the session that opened the PR.
    /// PR tracking lives on that root; follow-ups only point at it.
    /// Returns `None` if `id` (or any ancestor) does not exist.
    pub async fn resolve_tracking_root(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<Session>> {
        let Some(mut session) = self.repository.get(workspace, id).await? else {
            return Ok(None);
        };

        for _ in 0..MAX_FOLLOW_UP_DEPTH {
            let Some(parent_id) = session.parent_session_id.clone() else {
                return Ok(Some(session));
            };
            match self.repository.get(workspace, &parent_id).await? {
                Some(parent) => session = parent,
                None => anyhow::bail!("session {} has missing parent {parent_id}", session.id),
            }
        }

        anyhow::bail!("follow-up chain from {id} exceeds {MAX_FOLLOW_UP_DEPTH} levels")
    }

    pub async fn get_events(
        &self,
        workspace: &WorkspaceId,
        id: &str,
        after_seq: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<super::model::SessionEvent>> {
        self.repository
            .get_events(workspace, id, after_seq, limit)
            .await
    }

    pub async fn request_cancel(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<Session>> {
        self.repository.request_cancel(workspace, id).await
    }

    pub async fn expire_leases(&self) -> anyhow::Result<u64> {
        self.repository.expire_leases().await
    }

    pub async fn claim(&self, host_id: &str) -> anyhow::Result<Option<Session>> {
        self.repository.claim_pending(host_id).await
    }

    pub async fn heartbeat(&self, host_id: &str, id: &str) -> anyhow::Result<Option<bool>> {
        self.repository.heartbeat(host_id, id).await
    }

    pub async fn append_events(
        &self,
        host_id: &str,
        id: &str,
        events: &[super::model::NewSessionEvent],
    ) -> anyhow::Result<Option<()>> {
        self.repository.append_events(host_id, id, events).await
    }

    pub async fn finish(
        &self,
        host_id: &str,
        id: &str,
        status: SessionStatus,
        stop_reason: Option<String>,
        error: Option<String>,
        outcome: Option<SessionOutcome>,
    ) -> anyhow::Result<Option<Session>> {
        match status {
            SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled => {}
            _ => anyhow::bail!("finish status must be terminal, got {status:?}"),
        }

        self.repository
            .finish(host_id, id, status, stop_reason, error, outcome)
            .await
    }

    pub async fn pr_tracking_work_list(&self, limit: i64) -> anyhow::Result<Vec<Session>> {
        self.repository.pr_tracking_work_list(limit).await
    }

    pub async fn begin_pr_sync(&self, id: &str) -> anyhow::Result<Option<Box<dyn PrSync>>> {
        self.repository.begin_pr_sync(id).await
    }
}
