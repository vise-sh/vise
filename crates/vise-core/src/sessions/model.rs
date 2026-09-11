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
    /// "pr_opened" | "pushed_no_pr" | "uncommitted_changes" | "no_changes"
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
        let env = Environment { kind: "self_hosted".into(), repo: None, base_branch: None };
        assert!(env.validate().is_ok());
    }

    #[test]
    fn github_repo_requires_repo() {
        assert!(github_env(None, None).validate().is_err());
    }

    #[test]
    fn github_repo_accepts_owner_slash_name() {
        assert!(github_env(Some("vise-sh/vise-new"), None).validate().is_ok());
        assert!(github_env(Some("vise-sh/vise-new"), Some("main")).validate().is_ok());
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
            assert!(github_env(Some(bad), None).validate().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn unknown_kind_rejected() {
        let env = Environment { kind: "kubernetes".into(), repo: None, base_branch: None };
        assert!(env.validate().is_err());
    }
}
