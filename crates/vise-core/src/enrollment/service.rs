use std::sync::Arc;

use chrono::Utc;

use super::{model::EnrollmentToken, repository::EnrollmentTokenRepository};
use crate::hosts::repository::HostRepository;
use crate::hosts::service::{EnrolledHost, HostService, hash_token};
use crate::workspaces::model::WorkspaceId;

/// How many generated host names we try before giving up on an exchange. A
/// collision needs another host in the same workspace with the same prefix
/// *and* the same ~30-bit random suffix, so one retry is already rare.
const NAME_ATTEMPTS: usize = 8;

/// The name prefix used when the exchanging host does not send one.
const DEFAULT_NAME_PREFIX: &str = "host";

pub struct EnrollmentTokenService<R, H> {
    repository: R,
    hosts: Arc<HostService<H>>,
}

/// The plaintext secret is returned exactly once, at mint; only its hash is
/// stored.
pub struct MintedEnrollmentToken {
    pub token: EnrollmentToken,
    pub secret: String,
}

impl<R, H> EnrollmentTokenService<R, H>
where
    R: EnrollmentTokenRepository,
    H: HostRepository,
{
    pub fn new(repository: R, hosts: Arc<HostService<H>>) -> Self {
        Self { repository, hosts }
    }

    /// Mint an enrollment token in `workspace`. `max_uses`, when given, must
    /// be positive; the HTTP layer owns that validation (422), like the
    /// sessions routes do for environments.
    pub async fn mint(
        &self,
        workspace: WorkspaceId,
        max_uses: Option<i64>,
    ) -> anyhow::Result<MintedEnrollmentToken> {
        let secret = crate::id::new_token("venroll");

        let token = EnrollmentToken {
            id: crate::id::new_id("enr"),
            workspace_id: workspace,
            max_uses,
            uses: 0,
            revoked_at: None,
            created_at: Utc::now(),
        };

        let token = self.repository.create(token, &hash_token(&secret)).await?;

        Ok(MintedEnrollmentToken { token, secret })
    }

    pub async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<EnrollmentToken>> {
        self.repository.list(workspace).await
    }

    pub async fn revoke(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<EnrollmentToken>> {
        self.repository.revoke(workspace, id).await
    }

    /// Exchange an enrollment secret for a freshly enrolled ephemeral host
    /// in the token's workspace. The secret is the caller's only credential:
    /// `None` means it is unknown, revoked or at `max_uses`, without saying
    /// which. The host gets a generated `{prefix}-{random}` name; on the
    /// (rare) per-workspace name collision another suffix is tried.
    pub async fn exchange(
        &self,
        secret: &str,
        name_prefix: Option<&str>,
    ) -> anyhow::Result<Option<EnrolledHost>> {
        let Some(token) = self.repository.consume(&hash_token(secret)).await? else {
            return Ok(None);
        };

        let prefix = name_prefix
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or(DEFAULT_NAME_PREFIX);

        for _ in 0..NAME_ATTEMPTS {
            let name = format!("{prefix}-{}", crate::id::short_suffix());
            match self
                .hosts
                .enroll_ephemeral(token.workspace_id.clone(), name)
                .await
            {
                Ok(enrolled) => return Ok(Some(enrolled)),
                Err(error) if is_unique_violation(&error) => continue,
                Err(error) => return Err(error),
            }
        }

        anyhow::bail!(
            "could not find a free host name for prefix {prefix:?} in {NAME_ATTEMPTS} attempts"
        )
    }
}

/// Whether an error from a repository is a Postgres unique-constraint
/// violation (a generated host name already taken).
fn is_unique_violation(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(|error| error.as_database_error())
        .is_some_and(|database| database.is_unique_violation())
}
