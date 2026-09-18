use chrono::Utc;
use sha2::{Digest, Sha256};

use super::{model::Host, repository::HostRepository};
use crate::workspaces::model::WorkspaceId;

pub struct HostService<R> {
    repository: R,
}

/// The plaintext token is returned exactly once, at enrollment; only its hash
/// is stored.
pub struct EnrolledHost {
    pub host: Host,
    pub token: String,
}

impl<R> HostService<R>
where
    R: HostRepository,
{
    pub fn new(repository: R) -> Self {
        Self { repository }
    }

    pub async fn enroll(
        &self,
        workspace: WorkspaceId,
        name: String,
    ) -> anyhow::Result<EnrolledHost> {
        self.enroll_with(workspace, name, false).await
    }

    /// [`enroll`](Self::enroll) with `ephemeral = true`: the host is deleted
    /// by the reaper once it stops heartbeating. Used by the enrollment-token
    /// exchange; hand-enrolled hosts are never ephemeral.
    pub async fn enroll_ephemeral(
        &self,
        workspace: WorkspaceId,
        name: String,
    ) -> anyhow::Result<EnrolledHost> {
        self.enroll_with(workspace, name, true).await
    }

    async fn enroll_with(
        &self,
        workspace: WorkspaceId,
        name: String,
        ephemeral: bool,
    ) -> anyhow::Result<EnrolledHost> {
        let token = crate::id::new_token("vhost");

        let host = Host {
            id: crate::id::new_id("host"),
            workspace_id: workspace,
            name,
            ephemeral,
            last_seen_at: None,
            created_at: Utc::now(),
        };

        let host = self.repository.create(host, &hash_token(&token)).await?;

        Ok(EnrolledHost { host, token })
    }

    pub async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Host>> {
        self.repository.list(workspace).await
    }

    pub async fn authenticate(&self, token: &str) -> anyhow::Result<Option<Host>> {
        self.repository.authenticate(&hash_token(token)).await
    }

    /// Delete ephemeral hosts not seen since `ttl` ago, sparing any that
    /// still hold a running session. Returns how many were deleted.
    pub async fn reap_ephemeral(&self, ttl: std::time::Duration) -> anyhow::Result<u64> {
        let cutoff = Utc::now() - ttl;
        self.repository.delete_ephemeral_unseen_since(cutoff).await
    }
}

/// SHA-256 of a bearer secret, as lowercase hex: the only form a token is
/// stored or looked up in.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
