use std::collections::HashMap;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use anyhow::Error;
use k8s_managed_node_pool::cloud_provider::CloudProvider;
use k8s_managed_node_pool::model::{NodePool, NodePoolSettings, NodePoolTaint};

#[derive(Deserialize)]
pub struct DigitalOceanSettings {
    #[serde(rename = "DO_TOKEN")]
    pub token: String,
    #[serde(rename = "DO_CLUSTER_ID")]
    pub cluster_id: String,
}

pub struct DigitalOcean {
    client: Client,
    settings: DigitalOceanSettings,
}

impl DigitalOcean {
    pub fn new(settings: DigitalOceanSettings) -> Self {
        let client = Client::new();
        DigitalOcean { client, settings }
    }

    async fn list_pools(&self) -> Result<Vec<NodePool>, Error> {
        let cluster_id = &self.settings.cluster_id;
        let url =
            format!("https://api.digitalocean.com/v2/kubernetes/clusters/{cluster_id}/node_pools");
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.settings.token)
            .send()
            .await?;
        let result: DoPoolsResponse = response.json().await?;
        let pools = result.node_pools.into_iter().map(from_do_pool).collect();
        Ok(pools)
    }
}

impl CloudProvider for DigitalOcean {
    async fn find_pool_by_id(&self, id: &str) -> Result<Option<NodePool>, Error> {
        let cluster_id = &self.settings.cluster_id;
        let url = format!(
            "https://api.digitalocean.com/v2/kubernetes/clusters/{cluster_id}/node_pools/{id}"
        );
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.settings.token)
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            Ok(None)
        } else {
            let result: DoPoolResponse = response.json().await?;
            let result = from_do_pool(result.node_pool);
            Ok(Some(result))
        }
    }

    async fn find_pool_by_name(&self, name: &str) -> Result<Option<NodePool>, Error> {
        let pools = self.list_pools().await?;
        let pool = pools.into_iter().find(|pool| pool.settings.name == name);
        Ok(pool)
    }

    async fn create_pool(&self, settings: &NodePoolSettings) -> Result<NodePool, Error> {
        let cluster_id = &self.settings.cluster_id;
        let url =
            format!("https://api.digitalocean.com/v2/kubernetes/clusters/{cluster_id}/node_pools");
        let response = self
            .client
            .post(url)
            .json(&to_do_settings(settings))
            .bearer_auth(&self.settings.token)
            .send()
            .await?;

        let result: DoPoolResponse = response.json().await?;

        let pool = result.node_pool;
        let result = NodePool {
            id: pool.id,
            settings: from_do_pool_settings(pool.settings),
        };
        Ok(result)
    }

    async fn update_pool_by_id(
        &self,
        id: &str,
        settings: &NodePoolSettings,
    ) -> Result<NodePool, Error> {
        let cluster_id = &self.settings.cluster_id;
        let url = format!(
            "https://api.digitalocean.com/v2/kubernetes/clusters/{cluster_id}/node_pools/{id}"
        );
        let response = self
            .client
            .put(url)
            .json(&to_do_settings(settings))
            .bearer_auth(&self.settings.token)
            .send()
            .await?;

        let result: DoPoolResponse = response.json().await?;

        let pool = result.node_pool;
        let result = NodePool {
            id: pool.id,
            settings: from_do_pool_settings(pool.settings),
        };
        Ok(result)
    }

    async fn destroy_pool_by_id(&self, id: &str) -> Result<bool, Error> {
        let cluster_id = &self.settings.cluster_id;
        let url = format!(
            "https://api.digitalocean.com/v2/kubernetes/clusters/{cluster_id}/node_pools/{id}"
        );
        let response = self
            .client
            .delete(url)
            .bearer_auth(&self.settings.token)
            .send()
            .await?;

        if response.status() == StatusCode::NOT_FOUND {
            Ok(false)
        } else {
            response.error_for_status()?;
            Ok(true)
        }
    }
}

fn to_do_settings(settings: &NodePoolSettings) -> DoNodePoolSettings {
    DoNodePoolSettings {
        name: settings.name.clone(),
        size: settings.size.to_string(),
        count: settings.count,
        auto_scale: Some(settings.min_count.is_some() || settings.max_count.is_some()),
        min_nodes: settings.min_count,
        max_nodes: settings.max_count,
        tags: settings.tags.clone(),
        labels: settings.labels.clone(),
        taints: settings.taints.clone().map(|taints| {
            taints
                .into_iter()
                .map(|t| DoNodePoolTaint {
                    key: t.key,
                    value: t.value,
                    effect: t.effect,
                })
                .collect()
        }),
    }
}

fn from_do_pool_settings(settings: DoNodePoolSettings) -> NodePoolSettings {
    NodePoolSettings {
        name: settings.name,
        size: settings.size,
        count: settings.count,
        min_count: settings
            .min_nodes
            .filter(|_| settings.auto_scale == Some(true)),
        max_count: settings
            .max_nodes
            .filter(|_| settings.auto_scale == Some(true)),
        tags: settings.tags,
        labels: settings.labels,
        taints: settings.taints.map(|taints| {
            taints
                .into_iter()
                .map(|t| NodePoolTaint {
                    key: t.key,
                    value: t.value,
                    effect: t.effect,
                })
                .collect()
        }),
    }
}

fn from_do_pool(pool: DoNodePool) -> NodePool {
    NodePool {
        id: pool.id,
        settings: from_do_pool_settings(pool.settings),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DoPoolResponse {
    node_pool: DoNodePool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DoPoolsResponse {
    node_pools: Vec<DoNodePool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DoNodePool {
    id: String,
    #[serde(flatten)]
    settings: DoNodePoolSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct DoNodePoolSettings {
    name: String,
    size: String,
    count: u32,
    auto_scale: Option<bool>,
    min_nodes: Option<u32>,
    max_nodes: Option<u32>,
    tags: Option<Vec<String>>,
    labels: Option<HashMap<String, String>>,
    taints: Option<Vec<DoNodePoolTaint>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DoNodePoolTaint {
    pub key: String,
    pub value: String,
    pub effect: String,
}

#[cfg(test)]
mod tests {
    use k8s_managed_node_pool::{
        cloud_provider::CloudProvider,
        model::{NodePoolSettings, NodePoolTaint},
    };

    use super::DigitalOcean;

    const TEST_POOL_NAME: &str = "test-pool-123";

    #[tokio::test]
    #[ignore]
    async fn create_pool() {
        let provider = DigitalOcean::new(de_env::from_env().unwrap());
        let pool_settings = NodePoolSettings {
            name: TEST_POOL_NAME.to_string(),
            size: "s-1vcpu-2gb".to_string(),
            count: 2,
            taints: Some(vec![NodePoolTaint {
                key: "dedicated".to_string(),
                value: "test-123".to_string(),
                effect: "NoSchedule".to_string(),
            }]),
            min_count: None,
            max_count: None,
            tags: None,
            labels: None,
        };
        let pool = provider.create_pool(&pool_settings).await.unwrap();
        assert!(!pool.id.is_empty());
        assert_eq!(pool.settings.name, TEST_POOL_NAME);
    }

    #[tokio::test]
    #[ignore]
    async fn find_pool_by_name() {
        let provider = DigitalOcean::new(de_env::from_env().unwrap());
        let pool = provider.find_pool_by_name(TEST_POOL_NAME).await.unwrap();
        assert!(pool.is_some());
    }

    #[tokio::test]
    #[ignore]
    async fn update_pool() {
        let provider = DigitalOcean::new(de_env::from_env().unwrap());
        let mut pool = provider
            .find_pool_by_name(TEST_POOL_NAME)
            .await
            .unwrap()
            .unwrap();
        pool.settings.min_count = Some(1);
        pool.settings.max_count = Some(2);
        let pool = provider
            .update_pool_by_id(&pool.id, &pool.settings)
            .await
            .unwrap();
        assert_eq!(pool.settings.min_count, Some(1));
        assert_eq!(pool.settings.max_count, Some(2));
    }

    #[tokio::test]
    #[ignore]
    async fn destroy_pool() {
        let provider = DigitalOcean::new(de_env::from_env().unwrap());
        let pool = provider
            .find_pool_by_name(TEST_POOL_NAME)
            .await
            .unwrap()
            .unwrap();
        let res = provider.destroy_pool_by_id(&pool.id).await.unwrap();
        assert!(res);
    }
}
