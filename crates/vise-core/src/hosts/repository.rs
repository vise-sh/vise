use async_trait::async_trait;

use super::model::Host;
use crate::workspaces::model::WorkspaceId;

#[async_trait]
pub trait HostRepository: Send + Sync {
    /// Insert a host into the workspace named by `host.workspace_id`. Host
    /// names are unique within a workspace, not across workspaces.
    async fn create(&self, host: Host, token_hash: &str) -> anyhow::Result<Host>;

    async fn list(&self, workspace: &WorkspaceId) -> anyhow::Result<Vec<Host>>;

    /// Look up a host by its token hash, updating `last_seen_at` as a side
    /// effect. Returns `None` for unknown tokens. Not workspace-scoped: the
    /// token identifies the host, and the host row names its workspace.
    async fn authenticate(&self, token_hash: &str) -> anyhow::Result<Option<Host>>;
}
