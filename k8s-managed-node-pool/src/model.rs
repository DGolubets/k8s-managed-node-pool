use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

use duration_str::deserialize_option_duration;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[kube(
    kind = "ManagedNodePool",
    group = "dgolubets.github.io",
    version = "v1alpha1",
    namespaced,
    doc = "Managed node pool that gets created or destroyed automatically based on demand.",
    status = "ManagedNodePoolStatus"
)]
pub struct ManagedNodePoolSpec {
    #[serde(flatten)]
    pub settings: NodePoolSettings,

    #[serde(deserialize_with = "deserialize_option_duration")]
    #[schemars(with = "String")]
    pub idle_timeout: Option<Duration>,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema, Default)]
pub struct ManagedNodePoolStatus {
    pub node_pool_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_pool_status: Option<NodePoolStatus>,
    pub destroy_after: Option<SystemTime>,
}

#[derive(PartialEq, Deserialize, Serialize, Clone, Debug, JsonSchema)]
pub enum NodePoolStatus {
    CREATING,
    CREATED,
}

#[derive(Debug, Clone)]
pub struct NodePool {
    pub id: String,
    pub settings: NodePoolSettings,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct NodePoolSettings {
    pub name: String,
    pub size: String,
    pub count: u32,
    pub min_count: Option<u32>,
    pub max_count: Option<u32>,
    pub tags: Option<Vec<String>>,
    pub labels: Option<HashMap<String, String>>,
    pub taints: Option<Vec<NodePoolTaint>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct NodePoolTaint {
    pub key: String,
    pub value: String,
    pub effect: String,
}
