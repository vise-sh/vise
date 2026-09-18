use async_trait::async_trait;

use super::model::EnrollmentToken;
use crate::workspaces::model::WorkspaceId;

#[async_trait]
pub trait EnrollmentTokenRepository: Send + Sync {
    /// Insert a token into the workspace named by `token.workspace_id`.
    async fn create(
        &self,
        token: EnrollmentToken,
        token_hash: &str,
    ) -> anyhow::Result<EnrollmentToken>;

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<EnrollmentToken>>;

    /// Revoke a token. Idempotent: revoking an already-revoked token keeps
    /// its original `revoked_at`. Returns `None` when `id` does not exist in
    /// `workspace`.
    async fn revoke(
        &self,
        workspace: &WorkspaceId,
        id: &str,
    ) -> anyhow::Result<Option<EnrollmentToken>>;

    /// Atomically consume one use of the token with this hash: increments
    /// `uses` if and only if the token exists, is not revoked and is under
    /// `max_uses`. Check and increment happen in one statement, so
    /// concurrent exchanges cannot race past `max_uses`. Returns the token
    /// after the increment, or `None` when it could not be consumed (the
    /// reason is deliberately not distinguished: the hash is the caller's
    /// credential).
    async fn consume(&self, token_hash: &str) -> anyhow::Result<Option<EnrollmentToken>>;
}
