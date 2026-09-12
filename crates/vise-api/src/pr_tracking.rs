//! Background poller that keeps each `pr_opened` session's PR snapshot fresh
//! and records state transitions as session events.
//!
//! One tick claims sessions one at a time with `FOR UPDATE SKIP LOCKED`, so
//! several server instances split the work without coordination. Each claim
//! holds its row lock while GitHub is queried, and the snapshot update plus
//! event append commit in that same transaction.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use vise_core::sessions::model::{PrState, PrStatus, Session};
use vise_core::sessions::postgres::{PostgresSessionRepository, PrTrackingClaim};
use vise_core::sessions::pr_tracking::{
    self, CheckRunObservation, PrLifecycle, PrObservation, ReviewDecision, ReviewObservation,
    SYNC_ERROR_THRESHOLD, detect_transitions, parse_pr_url,
};

use crate::credentials::CredentialProvider;
use crate::github::{CheckRun, GitHubError, GitHubReadClient, PullRequest, Review};

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);

pub struct PrTracker {
    repository: PostgresSessionRepository,
    github: GitHubReadClient,
    credentials: Arc<dyn CredentialProvider>,
    interval: Duration,
}

/// What one tick did; the loop uses it to pick the next sleep.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Sessions whose snapshot was written (changed or merely re-synced).
    pub synced: usize,
    /// Sessions skipped because of a transient or credential error.
    pub skipped: usize,
    /// Set when GitHub rate-limited us; the rest of the tick was abandoned.
    pub backoff: Option<Duration>,
}

impl PrTracker {
    pub fn new(
        repository: PostgresSessionRepository,
        github: GitHubReadClient,
        credentials: Arc<dyn CredentialProvider>,
        interval: Duration,
    ) -> Self {
        Self {
            repository,
            github,
            credentials,
            interval,
        }
    }

    /// Poll forever. Never returns; every failure is logged and retried on
    /// the next tick.
    pub async fn run(self) {
        tracing::info!(
            interval_secs = self.interval.as_secs(),
            "pr tracking poller started"
        );
        loop {
            let report = self.tick().await;
            if report.synced > 0 || report.skipped > 0 {
                tracing::debug!(?report, "pr tracking tick");
            }
            let sleep = match report.backoff {
                Some(backoff) => {
                    tracing::warn!(?backoff, "github rate limited; backing off");
                    backoff.max(self.interval)
                }
                None => self.interval,
            };
            tokio::time::sleep(sleep).await;
        }
    }

    /// Sync every session in the work list once. Sessions synced during this
    /// tick are not revisited because their `last_synced_at` moves past the
    /// tick start; failed ones are excluded explicitly.
    pub async fn tick(&self) -> TickReport {
        let started = Utc::now();
        let mut report = TickReport::default();
        let mut handled: Vec<String> = Vec::new();
        let mut tokens: HashMap<String, String> = HashMap::new();

        loop {
            let claim = match self.repository.claim_pr_tracking(started, &handled).await {
                Ok(Some(claim)) => claim,
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(%error, "pr tracking: work list query failed");
                    break;
                }
            };

            let session_id = claim.session.id.clone();
            handled.push(session_id.clone());

            match self.sync(claim, &mut tokens).await {
                Ok(()) => report.synced += 1,
                Err(GitHubError::RateLimited { retry_after }) => {
                    report.backoff = Some(retry_after);
                    break;
                }
                Err(error) => {
                    report.skipped += 1;
                    tracing::warn!(%session_id, %error, "pr tracking: skipped session this tick");
                }
            }
        }

        report
    }

    async fn sync(
        &self,
        claim: PrTrackingClaim,
        tokens: &mut HashMap<String, String>,
    ) -> Result<(), GitHubError> {
        let session = claim.session.clone();
        let pr_url = session
            .outcome
            .as_ref()
            .and_then(|outcome| outcome.pr_url.clone())
            .unwrap_or_default();

        let Some((repo, number)) = parse_pr_url(&pr_url) else {
            // Nothing will ever make this URL readable; say so in the snapshot.
            tracing::warn!(session_id = %session.id, %pr_url, "unparseable pr_url");
            return self.write_sync_error(claim).await;
        };

        let token = match self.token_for(&session, &repo, tokens).await {
            Ok(token) => token,
            Err(error) => {
                self.release(claim).await;
                return Err(GitHubError::Transient(error));
            }
        };

        let observed = self.observe(&token, &repo, number).await;

        match observed {
            Ok(observation) => {
                let (state, checks) = pr_tracking::reduce(&observation);
                let events = detect_transitions(session.pr_status.as_ref(), state, checks);
                let status = PrStatus {
                    state,
                    checks,
                    last_synced_at: Utc::now(),
                };
                if !events.is_empty() {
                    tracing::info!(
                        session_id = %session.id,
                        pr_url = %pr_url,
                        state = state.as_str(),
                        checks = ?checks.map(|c| c.as_str()),
                        "pr state transition"
                    );
                }
                self.repository
                    .record_pr_status(claim, &status, &events)
                    .await
                    .map_err(GitHubError::Transient)
            }

            Err(GitHubError::Unreadable { status }) => {
                let claim = self
                    .repository
                    .record_pr_sync_failure(claim)
                    .await
                    .map_err(GitHubError::Transient)?;
                tracing::warn!(
                    session_id = %session.id,
                    %pr_url,
                    status,
                    failures = claim.sync_failures,
                    "pr unreadable"
                );
                if claim.sync_failures >= SYNC_ERROR_THRESHOLD {
                    self.write_sync_error(claim).await
                } else {
                    self.repository
                        .commit_pr_tracking(claim)
                        .await
                        .map_err(GitHubError::Transient)?;
                    Err(GitHubError::Unreadable { status })
                }
            }

            Err(error) => {
                self.release(claim).await;
                Err(error)
            }
        }
    }

    async fn observe(
        &self,
        token: &str,
        repo: &str,
        number: u64,
    ) -> Result<PrObservation, GitHubError> {
        let pr = self.github.pull_request(token, repo, number).await?;
        let reviews = self.github.reviews(token, repo, number).await?;
        let check_runs = self.github.check_runs(token, repo, &pr.head.sha).await?;
        Ok(observation(&pr, &reviews, &check_runs))
    }

    async fn write_sync_error(&self, claim: PrTrackingClaim) -> Result<(), GitHubError> {
        let previous = claim.session.pr_status.clone();
        let checks = previous.as_ref().and_then(|status| status.checks);
        let events = detect_transitions(previous.as_ref(), PrState::SyncError, checks);
        let status = PrStatus {
            state: PrState::SyncError,
            checks,
            last_synced_at: Utc::now(),
        };
        self.repository
            .record_pr_status(claim, &status, &events)
            .await
            .map_err(GitHubError::Transient)
    }

    async fn release(&self, claim: PrTrackingClaim) {
        if let Err(error) = self.repository.release_pr_tracking(claim).await {
            tracing::warn!(%error, "pr tracking: release failed");
        }
    }

    /// Installation tokens are per repo; mint once per repo per tick.
    async fn token_for(
        &self,
        session: &Session,
        repo: &str,
        tokens: &mut HashMap<String, String>,
    ) -> anyhow::Result<String> {
        if let Some(token) = tokens.get(repo) {
            return Ok(token.clone());
        }
        let issued = self
            .credentials
            .issue(session)
            .await
            .map_err(|error| match error {
                crate::credentials::IssueError::NotApplicable(reason) => {
                    anyhow::anyhow!("credential not applicable: {reason}")
                }
                crate::credentials::IssueError::Upstream(error) => error,
            })?;
        tokens.insert(repo.to_string(), issued.secret.clone());
        Ok(issued.secret)
    }
}

/// Convert GitHub payloads into the pure observation the reducer consumes.
pub fn observation(pr: &PullRequest, reviews: &[Review], check_runs: &[CheckRun]) -> PrObservation {
    let lifecycle = if pr.merged {
        PrLifecycle::Merged
    } else if pr.state == "open" {
        PrLifecycle::Open
    } else {
        PrLifecycle::Closed
    };

    PrObservation {
        lifecycle,
        head_sha: pr.head.sha.clone(),
        reviews: reviews
            .iter()
            .map(|review| ReviewObservation {
                reviewer: review
                    .user
                    .as_ref()
                    .map(|user| user.login.clone())
                    .unwrap_or_else(|| format!("review-{}", review.id)),
                decision: ReviewDecision::parse(&review.state),
                commit_id: review.commit_id.clone(),
                submitted_at: review.submitted_at,
            })
            .collect(),
        check_runs: check_runs
            .iter()
            .map(|run| CheckRunObservation {
                name: run.name.clone(),
                status: run.status.clone(),
                conclusion: run.conclusion.clone(),
            })
            .collect(),
    }
}
