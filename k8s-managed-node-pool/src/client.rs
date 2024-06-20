use anyhow::{anyhow, Error};
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::{
    api::{ListParams, Patch, PatchParams},
    Api, Client,
};

use crate::model::{ManagedNodePool, ManagedNodePoolStatus};

#[cfg(test)]
use mockall::automock;

#[trait_variant::make(Send)]
#[cfg_attr(test, automock)]
pub trait KubeClient {
    fn underlying_client(&self) -> &Client;
    async fn list_nodes_with_labels(&self, label_selector: &str) -> Result<Vec<Node>, Error>;
    async fn list_pods_with_labels(&self, label_selector: &str) -> Result<Vec<Pod>, Error>;
    async fn list_pods_with_fields(&self, field_selector: &str) -> Result<Vec<Pod>, Error>;
    async fn patch_pod(&self, pod: &Pod, manager: &str) -> Result<(), Error>;
    async fn patch_pool_status(
        &self,
        pool: &ManagedNodePool,
        status: &ManagedNodePoolStatus,
        manager: &str,
    ) -> Result<(), Error>;
}

#[derive(Clone)]
pub struct DefaultKubeClient {
    pub client: Client,
}

impl KubeClient for DefaultKubeClient {
    fn underlying_client(&self) -> &Client {
        &self.client
    }

    async fn list_nodes_with_labels(&self, label_selector: &str) -> Result<Vec<Node>, Error> {
        let api: Api<Node> = Api::all(self.client.clone());
        let nodes = api
            .list(&ListParams::default().labels(label_selector))
            .await?
            .items;
        Ok(nodes)
    }

    async fn list_pods_with_labels(&self, label_selector: &str) -> Result<Vec<Pod>, Error> {
        let api: Api<Pod> = Api::all(self.client.clone());

        let pods = api
            .list(&ListParams::default().labels(label_selector))
            .await?
            .items;

        Ok(pods)
    }

    async fn list_pods_with_fields(&self, field_selector: &str) -> Result<Vec<Pod>, Error> {
        let api: Api<Pod> = Api::all(self.client.clone());

        let pods = api
            .list(&ListParams::default().fields(field_selector))
            .await?
            .items;

        Ok(pods)
    }

    async fn patch_pod(&self, pod: &Pod, manager: &str) -> Result<(), Error> {
        let pod_name = pod
            .metadata
            .name
            .as_ref()
            .ok_or(anyhow!("Missing pod name"))?;

        let pod_namespace = pod
            .metadata
            .namespace
            .as_ref()
            .ok_or(anyhow!("Missing pod namespace"))?;

        let api: Api<Pod> = Api::namespaced(self.client.clone(), pod_namespace);
        let patch = Patch::Apply(&pod);
        let params = PatchParams::apply(manager);
        api.patch(pod_name, &params, &patch).await?;

        Ok(())
    }

    async fn patch_pool_status(
        &self,
        pool: &ManagedNodePool,
        status: &ManagedNodePoolStatus,
        manager: &str,
    ) -> Result<(), Error> {
        let pool_name = pool
            .metadata
            .name
            .as_ref()
            .ok_or(anyhow!("Missing pool name"))?;

        let pool_namespace = pool
            .metadata
            .namespace
            .as_ref()
            .ok_or(anyhow!("Missing pool namespace"))?;

        let api: Api<ManagedNodePool> = Api::namespaced(self.client.clone(), pool_namespace);
        let patch = serde_json::json!({
            "apiVersion": "dgolubets.github.io/v1alpha1",
            "kind": "ManagedNodePool",
            "status": status
        });
        let patch = Patch::Apply(&patch);
        let params = PatchParams::apply(manager);
        api.patch_status(pool_name, &params, &patch).await?;

        Ok(())
    }
}
