use async_trait::async_trait;

use super::model::Host;

#[async_trait]
pub trait HostRepository: Send + Sync {
    async fn create(&self, host: Host, token_hash: &str) -> anyhow::Result<Host>;

    async fn list(&self) -> anyhow::Result<Vec<Host>>;

    /// Look up a host by its token hash, updating `last_seen_at` as a side
    /// effect. Returns `None` for unknown tokens.
    async fn authenticate(&self, token_hash: &str) -> anyhow::Result<Option<Host>>;
}
