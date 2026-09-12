use async_trait::async_trait;

use super::model::{NewSessionEvent, PrStatus, Session, SessionOutcome, SessionStatus};

#[async_trait]
pub trait SessionRepository: Send + Sync {
    async fn create(&self, session: Session) -> anyhow::Result<Session>;

    async fn get(&self, id: &str) -> anyhow::Result<Option<Session>>;

    async fn list(&self) -> anyhow::Result<Vec<Session>>;

    async fn delete(&self, id: &str) -> anyhow::Result<()>;

    async fn get_events(
        &self,
        id: &str,
        after_seq: i64,
        limit: i64,
    ) -> anyhow::Result<Vec<super::model::SessionEvent>>;

    /// Cancel a pending session outright, or flag a running one for the host
    /// to act on. Returns `None` if the session is missing or already terminal.
    async fn request_cancel(&self, id: &str) -> anyhow::Result<Option<Session>>;

    /// Mark running sessions with lapsed leases as failed. Returns the number
    /// of sessions expired.
    async fn expire_leases(&self) -> anyhow::Result<u64>;

    /// Atomically claim the oldest pending session for a host. Returns `None` if no work.
    async fn claim_pending(&self, host_id: &str) -> anyhow::Result<Option<Session>>;

    /// Extend the lease on a running session held by this host.
    /// Returns `Some(cancel_requested)` or `None` if the host no longer holds the session.
    async fn heartbeat(&self, host_id: &str, id: &str) -> anyhow::Result<Option<bool>>;

    /// Append a batch of events for a running session held by this host.
    /// Idempotent on `(session_id, seq)`. Returns `None` if the host no longer holds the session.
    async fn append_events(
        &self,
        host_id: &str,
        id: &str,
        events: &[NewSessionEvent],
    ) -> anyhow::Result<Option<()>>;

    /// Move a running session held by this host to a terminal status.
    /// Returns `None` if the host no longer holds the session.
    async fn finish(
        &self,
        host_id: &str,
        id: &str,
        status: SessionStatus,
        stop_reason: Option<String>,
        error: Option<String>,
        outcome: Option<SessionOutcome>,
    ) -> anyhow::Result<Option<Session>>;

    /// The PR poller's work list: sessions whose outcome is `pr_opened` and
    /// whose PR snapshot is missing or non-terminal, least recently synced
    /// first.
    async fn pr_tracking_work_list(&self, limit: i64) -> anyhow::Result<Vec<Session>>;

    /// Lock one tracked session for a sync pass (`FOR UPDATE SKIP LOCKED`).
    /// Returns `None` when the session is not (or no longer) tracked, or
    /// when another poller currently holds it. Drop the handle to abort.
    async fn begin_pr_sync(&self, id: &str) -> anyhow::Result<Option<Box<dyn PrSync>>>;
}

/// An in-flight PR sync holding the session row lock. Committing writes the
/// new snapshot and appends the transition events in one transaction, so the
/// snapshot ("what is") and the event stream ("what happened") cannot drift.
#[async_trait]
pub trait PrSync: Send {
    fn session(&self) -> &Session;

    async fn commit(
        self: Box<Self>,
        status: PrStatus,
        events: Vec<serde_json::Value>,
    ) -> anyhow::Result<()>;
}
