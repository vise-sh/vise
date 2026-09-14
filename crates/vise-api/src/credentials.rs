use async_trait::async_trait;
use chrono::{DateTime, Utc};
use vise_core::sessions::model::Session;

pub struct IssuedCredential {
    pub secret: String,
    pub expires_at: Option<DateTime<Utc>>,
}

pub enum IssueError {
    /// Provider is configured but doesn't apply to this session → 422
    NotApplicable(String),
    /// Upstream (GitHub/etc.) failure → 502
    Upstream(anyhow::Error),
}

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    fn name(&self) -> &'static str;
    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError>;
}

/// The repository ("owner/name") a GitHub credential applies to, or
/// `NotApplicable` for sessions that do not run against a GitHub repository.
fn github_repo(session: &Session) -> Result<&str, IssueError> {
    session
        .environment
        .repo
        .as_deref()
        .filter(|_| session.environment.kind == "github_repo")
        .ok_or_else(|| IssueError::NotApplicable("session has no github_repo environment".into()))
}

/// Mints GitHub App installation tokens for github_repo sessions.
pub struct GithubCredentialProvider {
    pub client: std::sync::Arc<crate::github::GitHubAppClient>,
}

#[async_trait]
impl CredentialProvider for GithubCredentialProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError> {
        let repo = github_repo(session)?;

        let minted = self
            .client
            .installation_token(repo)
            .await
            .map_err(IssueError::Upstream)?;

        Ok(IssuedCredential {
            secret: minted.token,
            expires_at: Some(minted.expires_at),
        })
    }
}

/// Hands out one personal access token for every github_repo session. The
/// fallback for servers without the GitHub App; the PAT never expires from
/// the server's point of view, so `expires_at` is `None`.
pub struct PatCredentialProvider {
    pat: String,
}

impl PatCredentialProvider {
    pub fn new(pat: String) -> Self {
        Self { pat }
    }
}

#[async_trait]
impl CredentialProvider for PatCredentialProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError> {
        github_repo(session)?;
        Ok(IssuedCredential {
            secret: self.pat.clone(),
            expires_at: None,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use chrono::Utc;
    use vise_core::sessions::model::{Agent, Environment, Session, SessionStatus};

    /// A running session with the given environment, for provider tests.
    pub(crate) fn session(kind: &str, repo: Option<&str>) -> Session {
        Session {
            id: "ses_test".into(),
            agent: Agent {
                harness: "echo".into(),
                model: String::new(),
                instructions: String::new(),
                mcp_servers: vec![],
            },
            environment: Environment {
                kind: kind.into(),
                repo: repo.map(str::to_string),
                base_branch: None,
            },
            input: "do the thing".into(),
            status: SessionStatus::Running,
            host_id: Some("host_test".into()),
            lease_expires_at: None,
            started_at: None,
            finished_at: None,
            stop_reason: None,
            error: None,
            outcome: None,
            pr_status: None,
            parent_session_id: None,
            cancel_requested: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::session;
    use super::*;

    #[tokio::test]
    async fn pat_provider_issues_the_pat_for_github_repo_sessions() {
        let provider = PatCredentialProvider::new("github_pat_test".into());
        assert_eq!(provider.name(), "github");

        let issued = provider
            .issue(&session("github_repo", Some("acme/widgets")))
            .await
            .ok()
            .expect("issued");
        assert_eq!(issued.secret, "github_pat_test");
        assert_eq!(issued.expires_at, None, "a PAT does not expire on its own");
    }

    #[tokio::test]
    async fn pat_provider_rejects_sessions_without_a_github_repo() {
        let provider = PatCredentialProvider::new("github_pat_test".into());

        for session in [
            session("self_hosted", None),
            session("self_hosted", Some("acme/widgets")),
            session("github_repo", None),
        ] {
            let kind = session.environment.kind.clone();
            match provider.issue(&session).await {
                Err(IssueError::NotApplicable(_)) => {}
                Err(IssueError::Upstream(error)) => panic!("{kind}: upstream error {error}"),
                Ok(_) => panic!("{kind}: should not issue"),
            }
        }
    }
}
