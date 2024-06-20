use crate::model::NodePool;
use crate::model::NodePoolSettings;
use anyhow::Error;

#[cfg(test)]
use mockall::automock;
#[trait_variant::make(Send)]
#[cfg_attr(test, automock)]
pub trait CloudProvider {
    async fn find_pool_by_id(&self, id: &str) -> Result<Option<NodePool>, Error>;
    async fn find_pool_by_name(&self, name: &str) -> Result<Option<NodePool>, Error>;
    async fn create_pool(&self, settings: &NodePoolSettings) -> Result<NodePool, Error>;
    async fn update_pool_by_id(
        &self,
        id: &str,
        settings: &NodePoolSettings,
    ) -> Result<NodePool, Error>;
    async fn destroy_pool_by_id(&self, id: &str) -> Result<bool, Error>;
}
