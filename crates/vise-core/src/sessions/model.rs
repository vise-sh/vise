use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Agent {
    pub harness: String,
    pub model: String,
    pub instructions: String,
    pub mcp_servers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Environment {
    /// "self_hosted" or "github_repo"
    pub kind: String,
    /// Required when kind == "github_repo": "owner/name"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Branch to base work on; None = repo default branch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

impl Environment {
    pub fn validate(&self) -> Result<(), String> {
        match self.kind.as_str() {
            "self_hosted" => Ok(()),
            "github_repo" => {
                let repo = self.repo.as_deref().ok_or("github_repo requires `repo`")?;
                let valid_segment = |s: &str| {
                    !s.is_empty()
                        && s.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                };
                let mut parts = repo.split('/');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some(owner), Some(name), None)
                        if valid_segment(owner) && valid_segment(name) =>
                    {
                        Ok(())
                    }
                    _ => Err(format!("repo must be \"owner/name\", got {repo:?}")),
                }
            }
            other => Err(format!("unknown environment kind {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionOutcome {
    /// "pr_opened" | "pr_updated" | "pushed_no_pr" | "uncommitted_changes" | "no_changes"
    ///
    /// `pr_updated` is reported by follow-up sessions that pushed to the
    /// existing branch of a PR opened by an ancestor session.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Derived review state of a session's pull request. Reduced from GitHub's
/// review list (see `pr_tracking::reduce`); raw reviews are never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    ReviewPending,
    ChangesRequested,
    Approved,
    Merged,
    Closed,
    /// The poller could not read the PR (persistent 403/404). Retried on
    /// every tick; clears once a fetch succeeds.
    SyncError,
}

impl PrState {
    /// Merged and closed PRs leave the poller's work list forever.
    pub fn is_terminal(self) -> bool {
        matches!(self, PrState::Merged | PrState::Closed)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PrState::ReviewPending => "review_pending",
            PrState::ChangesRequested => "changes_requested",
            PrState::Approved => "approved",
            PrState::Merged => "merged",
            PrState::Closed => "closed",
            PrState::SyncError => "sync_error",
        }
    }
}

/// Derived state of the check runs on the PR's head commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChecksState {
    Pending,
    Passing,
    Failing,
}

impl ChecksState {
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksState::Pending => "pending",
            ChecksState::Passing => "passing",
            ChecksState::Failing => "failing",
        }
    }
}

/// Snapshot of what the PR looks like right now. Transitions between
/// snapshots are appended to the session's event stream as
/// `pr_state_changed` / `checks_state_changed` events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PrStatus {
    pub state: PrState,
    /// None when the head commit has no check runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<ChecksState>,
    pub last_synced_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    pub agent: Agent,
    pub environment: Environment,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Session {
    pub id: String,
    pub agent: Agent,
    pub environment: Environment,
    pub input: String,
    pub status: SessionStatus,
    pub host_id: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub stop_reason: Option<String>,
    pub error: Option<String>,
    pub outcome: Option<SessionOutcome>,
    /// PR tracking snapshot; only present once the poller has synced a
    /// session whose outcome is `pr_opened`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_status: Option<PrStatus>,
    /// Set on follow-up sessions: the session whose PR this one addresses.
    /// Follow-ups chain; PR tracking always lives on the root session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub cancel_requested: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NewSessionEvent {
    pub seq: i64,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SessionEvent {
    pub session_id: String,
    pub seq: i64,
    pub payload: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github_env(repo: Option<&str>, base: Option<&str>) -> Environment {
        Environment {
            kind: "github_repo".into(),
            repo: repo.map(String::from),
            base_branch: base.map(String::from),
        }
    }

    #[test]
    fn self_hosted_needs_no_repo() {
        let env = Environment {
            kind: "self_hosted".into(),
            repo: None,
            base_branch: None,
        };
        assert!(env.validate().is_ok());
    }

    #[test]
    fn github_repo_requires_repo() {
        assert!(github_env(None, None).validate().is_err());
    }

    #[test]
    fn github_repo_accepts_owner_slash_name() {
        assert!(
            github_env(Some("vise-sh/vise-new"), None)
                .validate()
                .is_ok()
        );
        assert!(
            github_env(Some("vise-sh/vise-new"), Some("main"))
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn github_repo_rejects_bad_formats() {
        for bad in [
            "vise-sh",
            "a/b/c",
            "",
            "owner/",
            "/name",
            "https://github.com/a/b",
            "own er/name",
            "owner/na?me",
        ] {
            assert!(
                github_env(Some(bad), None).validate().is_err(),
                "{bad:?} should fail"
            );
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        let env = Environment {
            kind: "kubernetes".into(),
            repo: None,
            base_branch: None,
        };
        assert!(env.validate().is_err());
    }
}
