//! Background PR tracking.
//!
//! Every tick the poller takes the sessions whose PR is still open, observes
//! each PR on GitHub, reduces the observation to a [`PrStatus`] and, when it
//! differs from the stored snapshot, writes the new snapshot together with
//! the transition events in one transaction. A quiet tick only touches
//! `last_synced_at`.
//!
//! Multiple server instances split the work through the row lock taken by
//! `begin_pr_sync` (`FOR UPDATE SKIP LOCKED`); transitions are compare-then-
//! write against the locked snapshot, so a double poll is harmless anyway.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use vise_core::sessions::model::{PrState, PrStatus, Session};
use vise_core::sessions::pr_tracking::{reduce, transitions};
use vise_core::sessions::repository::SessionRepository;
use vise_core::sessions::service::SessionService;

use crate::github::{GitHubApi, PullRef};

/// Consecutive 403/404 responses before a session is marked `sync_error`.
/// One-off blips (token rotation, replication lag) retry silently.
pub const SYNC_ERROR_THRESHOLD: u32 = 3;

/// Upper bound on sessions processed per tick per instance.
const WORK_LIST_LIMIT: i64 = 200;

/// How long to wait when GitHub rate-limits us and gives no reset time.
const DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Sessions whose snapshot was written (changed or merely touched).
    pub synced: usize,
    /// Sessions skipped: locked elsewhere, fetch failed, no credential.
    pub skipped: usize,
    /// Set when GitHub rate-limited the tick; the rest of the work list was
    /// left for the next tick and the caller should wait at least this long.
    pub backoff: Option<Duration>,
}

pub struct PrPoller<R> {
    sessions: Arc<SessionService<R>>,
    /// GitHub read client, authenticated with the App or the PAT; `None`
    /// when neither is configured, which disables tracking.
    github: Option<GitHubApi>,
    interval: Duration,
    /// Consecutive not-visible failures per session id.
    failures: HashMap<String, u32>,
}

enum SyncFailure {
    /// Row locked by another instance, or no longer tracked.
    Skipped,
    RateLimited(Option<DateTime<Utc>>),
}

impl<R> PrPoller<R>
where
    R: SessionRepository + 'static,
{
    pub fn new(
        sessions: Arc<SessionService<R>>,
        github: Option<GitHubApi>,
        interval: Duration,
    ) -> Self {
        Self {
            sessions,
            github,
            interval,
            failures: HashMap::new(),
        }
    }

    /// Poll forever. Never returns while the server runs: every failure is
    /// logged and retried on the next tick.
    pub async fn run_forever(mut self) {
        let Some(github) = &self.github else {
            tracing::warn!("github app or pat not configured; PR tracking disabled");
            return;
        };

        tracing::info!(
            interval_secs = self.interval.as_secs(),
            auth = github.auth().kind(),
            "pr poller started"
        );

        loop {
            let report = self.tick().await;
            if report.synced > 0 || report.skipped > 0 {
                tracing::debug!(
                    synced = report.synced,
                    skipped = report.skipped,
                    "pr poll tick"
                );
            }

            let wait = match report.backoff {
                Some(backoff) => {
                    tracing::warn!(secs = backoff.as_secs(), "github rate limited; backing off");
                    backoff.max(self.interval)
                }
                None => self.interval,
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// One pass over the work list.
    pub async fn tick(&mut self) -> TickReport {
        let mut report = TickReport::default();

        let work = match self.sessions.pr_tracking_work_list(WORK_LIST_LIMIT).await {
            Ok(work) => work,
            Err(error) => {
                tracing::error!(%error, "pr tracking work list query failed");
                return report;
            }
        };

        for session in work {
            match self.sync_session(&session.id).await {
                Ok(()) => report.synced += 1,
                Err(SyncFailure::Skipped) => report.skipped += 1,
                Err(SyncFailure::RateLimited(reset_at)) => {
                    report.skipped += 1;
                    let until_reset = reset_at
                        .and_then(|reset| (reset - Utc::now()).to_std().ok())
                        .unwrap_or(DEFAULT_RATE_LIMIT_BACKOFF);
                    report.backoff = Some(until_reset);
                    break;
                }
            }
        }

        report
    }

    async fn sync_session(&mut self, id: &str) -> Result<(), SyncFailure> {
        let Some(github) = self.github.clone() else {
            return Err(SyncFailure::Skipped);
        };

        let sync = match self.sessions.begin_pr_sync(id).await {
            Ok(Some(sync)) => sync,
            Ok(None) => return Err(SyncFailure::Skipped),
            Err(error) => {
                tracing::error!(session_id = %id, %error, "pr sync lock failed");
                return Err(SyncFailure::Skipped);
            }
        };

        let session = sync.session().clone();
        let previous = session.pr_status.clone();

        let Some(pr) = pr_ref(&session) else {
            // Nothing to retry: the URL will not become parseable later.
            tracing::error!(session_id = %id, "session outcome has no parseable pr_url");
            return write_state(sync, previous.as_ref(), PrState::SyncError)
                .await
                .map_err(|_| SyncFailure::Skipped);
        };

        match github.observe(&pr).await {
            Ok((_, observation)) => {
                self.failures.remove(id);
                let (state, checks) = reduce(&observation);
                let events: Vec<serde_json::Value> = transitions(previous.as_ref(), state, checks)
                    .iter()
                    .map(|t| t.to_event_payload())
                    .collect();
                let status = PrStatus {
                    state,
                    checks,
                    last_synced_at: Utc::now(),
                };
                if !events.is_empty() {
                    tracing::info!(session_id = %id, ?state, ?checks, "pr state changed");
                }
                sync.commit(status, events).await.map_err(|error| {
                    tracing::error!(session_id = %id, %error, "pr snapshot write failed");
                    SyncFailure::Skipped
                })
            }

            Err(error) => {
                if let Some(reset_at) = error.rate_limit_reset() {
                    return Err(SyncFailure::RateLimited(reset_at));
                }

                if error.is_not_visible() {
                    let count = self.failures.entry(id.to_string()).or_insert(0);
                    *count += 1;
                    if *count >= SYNC_ERROR_THRESHOLD {
                        tracing::error!(session_id = %id, %error, attempts = *count, "pr not readable; marking sync_error");
                        return write_state(sync, previous.as_ref(), PrState::SyncError)
                            .await
                            .map_err(|_| SyncFailure::Skipped);
                    }
                }

                tracing::warn!(session_id = %id, %error, "pr fetch failed; will retry");
                Err(SyncFailure::Skipped)
            }
        }
    }
}

fn pr_ref(session: &Session) -> Option<PullRef> {
    session
        .outcome
        .as_ref()
        .and_then(|outcome| outcome.pr_url.as_deref())
        .and_then(PullRef::parse)
}

/// Write `state` keeping the previously observed checks (we learned nothing
/// new about them), appending a transition event if the state changed.
async fn write_state(
    sync: Box<dyn vise_core::sessions::repository::PrSync>,
    previous: Option<&PrStatus>,
    state: PrState,
) -> anyhow::Result<()> {
    let checks = previous.and_then(|p| p.checks);
    let events: Vec<serde_json::Value> = transitions(previous, state, checks)
        .iter()
        .map(|t| t.to_event_payload())
        .collect();
    sync.commit(
        PrStatus {
            state,
            checks,
            last_synced_at: Utc::now(),
        },
        events,
    )
    .await
}
