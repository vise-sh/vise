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
        let token = crate::id::new_token("vhost");

        let host = Host {
            id: crate::id::new_id("host"),
            workspace_id: workspace,
            name,
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
}

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
