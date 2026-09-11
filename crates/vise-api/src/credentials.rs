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

/// v1: mints GitHub App installation tokens for github_repo sessions.
pub struct GithubCredentialProvider {
    pub client: crate::github::GitHubAppClient,
}

#[async_trait]
impl CredentialProvider for GithubCredentialProvider {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn issue(&self, session: &Session) -> Result<IssuedCredential, IssueError> {
        let repo = session
            .environment
            .repo
            .as_deref()
            .filter(|_| session.environment.kind == "github_repo")
            .ok_or_else(|| {
                IssueError::NotApplicable("session has no github_repo environment".into())
            })?;

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
