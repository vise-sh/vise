use chrono::Utc;

use super::{
    model::{Session, SessionOutcome, SessionStatus},
    repository::SessionRepository,
};

pub struct SessionService<R> {
    repository: R,
}

impl<R> SessionService<R>
where
    R: SessionRepository,
{
    pub fn new(repository: R) -> Self {
        Self { repository }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Session>> {
        self.repository.list().await
    }

    pub async fn get(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.repository.get(id).await
    }

    pub async fn create(
        &self,
        agent: super::model::Agent,
        environment: super::model::Environment,
        input: String,
    ) -> anyhow::Result<Session> {
        self.create_with_parent(agent, environment, input, None)
            .await
    }

    /// Create a session, optionally as a follow-up to `parent_session_id`.
    pub async fn create_with_parent(
        &self,
        agent: super::model::Agent,
        environment: super::model::Environment,
        input: String,
        parent_session_id: Option<String>,
    ) -> anyhow::Result<Session> {
        let now = Utc::now();

        let session = Session {
            id: crate::id::new_id("ses"),
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
            cancel_requested: false,
            parent_session_id,
            pr_status: None,
            created_at: now,
            updated_at: now,
        };

        self.repository.create(session).await
    }

    /// Follow the `parent_session_id` chain from `id` to the session that
    /// opened the PR. Returns `None` if `id` (or a link in the chain) is
    /// missing. PR tracking always lives on the root, so follow-up validation
    /// and input composition read from it.
    pub async fn resolve_root(&self, id: &str) -> anyhow::Result<Option<Session>> {
        // Chains are short in practice; the bound guards against a cycle that
        // could only be introduced by hand-editing the database.
        const MAX_DEPTH: usize = 64;

        let mut current = match self.repository.get(id).await? {
            Some(session) => session,
            None => return Ok(None),
        };

        for _ in 0..MAX_DEPTH {
            let Some(parent_id) = current.parent_session_id.clone() else {
                return Ok(Some(current));
            };
            current = match self.repository.get(&parent_id).await? {
                Some(session) => session,
                None => anyhow::bail!("follow-up chain broken: parent {parent_id} missing"),
            };
        }

        anyhow::bail!("follow-up chain from {id} exceeds {MAX_DEPTH} links")
    }

    pub async fn get_events(
        &self,
        id: &str,
        after_seq: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<super::model::SessionEvent>> {
        self.repository.get_events(id, after_seq, limit).await
    }

    pub async fn request_cancel(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.repository.request_cancel(id).await
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
}
