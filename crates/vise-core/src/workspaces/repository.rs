use async_trait::async_trait;

use super::model::{Workspace, WorkspaceId};

/// Storage for workspaces. The single-tenant server never calls this (the
/// migrations seed the `default` workspace); it exists so a hosting layer can
/// create tenants against the same schema.
#[async_trait]
pub trait WorkspaceRepository: Send + Sync {
    async fn create(&self, workspace: Workspace) -> anyhow::Result<Workspace>;

    async fn get(&self, id: &WorkspaceId) -> anyhow::Result<Option<Workspace>>;

    async fn list(&self) -> anyhow::Result<Vec<Workspace>>;

    /// Delete a workspace. Fails while any host or session still references
    /// it; the caller is expected to drain those first.
    async fn delete(&self, id: &WorkspaceId) -> anyhow::Result<()>;
}
