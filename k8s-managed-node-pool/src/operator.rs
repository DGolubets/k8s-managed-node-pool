use std::{
    collections::{BTreeMap, HashMap},
    ops::Add,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::{
    client::KubeClient,
    cloud_provider::CloudProvider,
    model::{
        ManagedNodePool, ManagedNodePoolStatus, NodePoolSettings, NodePoolStatus, NodePoolTaint,
    },
};
use anyhow::{format_err, Error};
use futures::{StreamExt, TryFutureExt};
use k8s_openapi::api::core::v1::{Pod, PodSpec, Toleration};
use kube::{
    api::ObjectMeta,
    runtime::{
        controller::{self, Action},
        finalizer::{self, finalizer, Event},
        reflector::ObjectRef,
        watcher, Controller,
    },
    Api, Resource, ResourceExt,
};

const LABEL_MANAGED_NODE_POOL: &str = "dgolubets.github.io/managed-node-pool";
const FINALIZER_NAME: &str = "dgolubets.github.io/managed-node-pool-cleanup";
const FIELD_SELECTOR_PENDING: &str = "status.phase==Pending";

#[derive(thiserror::Error, Debug)]
enum OperatorError {
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

pub struct Operator<K, P> {
    pub client: Arc<K>,
    pub cloud_provider: Arc<P>,
}

impl<K, P> Operator<K, P>
where
    K: KubeClient + Sync + 'static,
    P: CloudProvider + Sync + 'static,
{
    pub async fn run(self) -> Result<(), Error> {
        let main_api: Api<ManagedNodePool> = Api::all(self.client.underlying_client().clone());
        let pods_api: Api<Pod> = Api::all(self.client.underlying_client().clone());
        Controller::new(main_api, Default::default())
            .watches(
                pods_api.clone(),
                watcher::Config::default().labels(LABEL_MANAGED_NODE_POOL),
                |pod| find_pool_ref(&pod),
            )
            .watches(
                pods_api,
                watcher::Config::default().fields(FIELD_SELECTOR_PENDING),
                |pod| find_pool_ref(&pod),
            )
            .with_config(controller::Config::default().debounce(Duration::from_secs(5)))
            .run(
                |p, c| c.reconcile_or_cleanup(p),
                |p, e, c| c.error_policy(p, e),
                Arc::new(self),
            )
            .for_each(|_| futures::future::ready(()))
            .await;
        Ok(())
    }
}

impl<K, P> Operator<K, P>
where
    K: KubeClient,
    P: CloudProvider,
{
    async fn reconcile_or_cleanup(
        self: Arc<Self>,
        pool: Arc<ManagedNodePool>,
    ) -> Result<Action, finalizer::Error<OperatorError>> {
        let ns = pool
            .metadata
            .namespace
            .as_deref()
            .ok_or(finalizer::Error::UnnamedObject)?;

        let api: Api<ManagedNodePool> =
            Api::namespaced(self.client.underlying_client().clone(), ns);

        finalizer(&api, FINALIZER_NAME, pool, |event| async {
            match event {
                Event::Apply(p) => self.reconcile(p).map_err(OperatorError::Anyhow).await,
                Event::Cleanup(p) => self.cleanup(p).map_err(OperatorError::Anyhow).await,
            }
        })
        .await
    }

    async fn reconcile(self: Arc<Self>, pool: Arc<ManagedNodePool>) -> Result<Action, Error> {
        let mut node_pool = if let Some(status) = &pool.status {
            if let Some(id) = &status.node_pool_id {
                tracing::debug!("Getting existing node pool..");
                if let Some(pool_id) = self.cloud_provider.find_pool_by_id(id).await? {
                    Some(pool_id)
                } else {
                    tracing::error!("Failed to find nodepool by id. Resetting status..");
                    self.patch_status(&pool, Default::default()).await?;
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        if let Some(node_pool) = &node_pool {
            if needs_update(&pool.spec.settings, &node_pool.settings) {
                tracing::debug!("Updating node pool settings..");
                let settings = patch_pool_settings(pool.spec.settings.clone(), &pool)?;
                self.cloud_provider
                    .update_pool_by_id(&node_pool.id, &settings)
                    .await?;
            }
        }

        if let Some(ManagedNodePoolStatus {
            node_pool_id: None,
            node_pool_status: Some(NodePoolStatus::CREATING),
            destroy_after: _,
        }) = &pool.status
        {
            tracing::warn!("Pool is in CREATING state, but no pool id is recorded. Syncing..");

            node_pool = if let Some(node_pool) = self
                .cloud_provider
                .find_pool_by_name(&pool.spec.settings.name)
                .await?
            {
                self.patch_status(
                    &pool,
                    ManagedNodePoolStatus {
                        node_pool_id: Some(node_pool.id.clone()),
                        node_pool_status: Some(NodePoolStatus::CREATED),
                        ..Default::default()
                    },
                )
                .await?;
                Some(node_pool)
            } else {
                tracing::error!("Failed to find nodepool by name. Resetting status..");
                self.patch_status(&pool, Default::default()).await?;
                None
            }
        }

        tracing::debug!("Listing podes..");
        let pods = self.list_pods(&pool).await?;

        for pod in &pods {
            if !pod.labels().contains_key(LABEL_MANAGED_NODE_POOL) {
                tracing::debug!("Patching pod {}..", pod.name_any());
                self.patch_pod(pod, &pool).await?;
            }
        }

        if node_pool.is_some() && pods.is_empty() {
            tracing::debug!("Node pool is idle.");

            if let Some(
                status @ ManagedNodePoolStatus {
                    node_pool_id: Some(node_pool_id),
                    node_pool_status: _,
                    destroy_after,
                },
            ) = &pool.status
            {
                if let Some(idle_timeout) = pool.spec.idle_timeout {
                    if let Some(destroy_after) = destroy_after {
                        if SystemTime::now() < *destroy_after {
                            let duration = destroy_after
                                .duration_since(SystemTime::now())
                                .unwrap_or(Duration::from_secs(0));
                            return Ok(Action::requeue(duration));
                        }
                    } else {
                        tracing::info!(
                            "Scheduling the pool shutdown down in {} s..",
                            idle_timeout.as_secs()
                        );
                        let mut status = status.clone();
                        status.destroy_after = Some(SystemTime::now().add(idle_timeout));
                        self.patch_status(&pool, status).await?;
                        return Ok(Action::requeue(idle_timeout));
                    }
                }

                tracing::info!("Shutting the pool down..");
                self.cloud_provider.destroy_pool_by_id(node_pool_id).await?;
                self.patch_status(&pool, Default::default()).await?;
            } else {
                tracing::warn!("Cannot shutdown the pool due to missing status information.");
            }
        }

        if node_pool.is_none() && !pods.is_empty() {
            tracing::debug!(
                "Node pool is not started and there are {} waiting pods.",
                pods.len()
            );
            tracing::info!("Creating a pool..");
            self.patch_status(
                &pool,
                ManagedNodePoolStatus {
                    node_pool_id: None,
                    node_pool_status: Some(NodePoolStatus::CREATING),
                    ..Default::default()
                },
            )
            .await?;
            let settings = patch_pool_settings(pool.spec.settings.clone(), &pool)?;
            let node_pool = self.cloud_provider.create_pool(&settings).await?;
            self.patch_status(
                &pool,
                ManagedNodePoolStatus {
                    node_pool_id: Some(node_pool.id),
                    node_pool_status: Some(NodePoolStatus::CREATED),
                    ..Default::default()
                },
            )
            .await?;
        }

        Ok(Action::requeue(Duration::from_secs(3600)))
    }

    async fn cleanup(self: Arc<Self>, pool: Arc<ManagedNodePool>) -> Result<Action, Error> {
        tracing::debug!("Running cleanup..");
        if let Some(status) = &pool.status {
            if let Some(id) = &status.node_pool_id {
                tracing::info!("Destroying node pool..");
                self.cloud_provider.destroy_pool_by_id(id).await?;
            }
        }

        Ok(Action::await_change())
    }

    fn error_policy(
        self: Arc<Self>,
        _pool: Arc<ManagedNodePool>,
        err: &finalizer::Error<OperatorError>,
    ) -> Action {
        tracing::error!("Error: {}", err);
        Action::requeue(Duration::from_secs(5))
    }

    async fn list_pods(&self, pool: &ManagedNodePool) -> Result<Vec<Pod>, Error> {
        let labeled_pods = self
            .client
            .list_pods_with_labels(LABEL_MANAGED_NODE_POOL)
            .await?;

        let pending_pods = self
            .client
            .list_pods_with_fields(FIELD_SELECTOR_PENDING)
            .await?;

        let all_pods = labeled_pods
            .into_iter()
            .chain(pending_pods.into_iter().filter(|pod| {
                find_pool_ref(pod)
                    .iter()
                    .any(|obj_ref| matches_pool_ref(pool, obj_ref))
            }))
            .collect();

        Ok(all_pods)
    }

    async fn patch_pod(&self, pod: &Pod, pool: &ManagedNodePool) -> Result<(), Error> {
        let patch = Pod {
            metadata: ObjectMeta {
                name: pod.metadata.name.clone(),
                namespace: pod.metadata.namespace.clone(),
                labels: Some({
                    let mut map = BTreeMap::new();
                    map.insert(
                        LABEL_MANAGED_NODE_POOL.to_string(),
                        get_pool_full_name(pool)?.to_string(),
                    );
                    map
                }),
                ..Default::default()
            },
            spec: Some(PodSpec {
                tolerations: Some(vec![Toleration {
                    effect: Some("NoSchedule".to_string()),
                    key: Some(LABEL_MANAGED_NODE_POOL.to_string()),
                    operator: Some("Equal".to_string()),
                    value: Some(get_pool_taint(pool)?.to_string()),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        };

        self.client.patch_pod(&patch, LABEL_MANAGED_NODE_POOL).await
    }

    async fn patch_status(
        &self,
        pool: &ManagedNodePool,
        status: ManagedNodePoolStatus,
    ) -> Result<(), Error> {
        self.client
            .patch_pool_status(pool, &status, LABEL_MANAGED_NODE_POOL)
            .await
    }
}

fn needs_update(new_settings: &NodePoolSettings, current_settings: &NodePoolSettings) -> bool {
    // todo: handle more updated settings
    new_settings.count != current_settings.count
        || new_settings.min_count != current_settings.min_count
        || new_settings.max_count != current_settings.max_count
}

fn patch_pool_settings(
    mut settings: NodePoolSettings,
    pool: &ManagedNodePool,
) -> Result<NodePoolSettings, Error> {
    settings.taints.get_or_insert(vec![]).push(NodePoolTaint {
        key: LABEL_MANAGED_NODE_POOL.to_string(),
        value: get_pool_taint(pool)?.to_string(),
        effect: "NoSchedule".to_string(),
    });
    settings.labels.get_or_insert(HashMap::new()).insert(
        LABEL_MANAGED_NODE_POOL.to_string(),
        get_pool_full_name(&pool)?.to_string(),
    );

    Ok(settings)
}

fn find_pool_ref(pod: &Pod) -> Option<ObjectRef<ManagedNodePool>> {
    let pool_name = pod.labels().get(LABEL_MANAGED_NODE_POOL).or_else(|| {
        let spec = pod.spec.as_ref()?;
        let node_selector = spec.node_selector.as_ref()?;
        node_selector.get(LABEL_MANAGED_NODE_POOL)
    })?;

    let (pool_name, pool_namespace) = pool_name.split_once('.')?;
    Some(ObjectRef::new(pool_name).within(pool_namespace))
}

fn matches_pool_ref(pool: &ManagedNodePool, obj_ref: &ObjectRef<ManagedNodePool>) -> bool {
    pool.metadata.name.as_ref() == Some(&obj_ref.name)
        && pool.metadata.namespace == obj_ref.namespace
}

fn get_pool_full_name(pool: &ManagedNodePool) -> Result<String, Error> {
    let name = format!(
        "{}.{}",
        pool.meta()
            .name
            .as_ref()
            .ok_or(format_err!("Missing pool name"))?,
        pool.meta()
            .namespace
            .as_ref()
            .ok_or(format_err!("Missing pool namespace"))?
    );
    Ok(name)
}

fn get_pool_taint(pool: &ManagedNodePool) -> Result<&str, Error> {
    let uid = pool
        .meta()
        .uid
        .as_deref()
        .ok_or(format_err!("Missing pool UID"))?;
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use mockall::predicate::eq;

    use crate::{
        client::MockKubeClient,
        cloud_provider::MockCloudProvider,
        model::{ManagedNodePoolSpec, NodePool, NodePoolSettings},
    };

    use super::*;

    impl Default for NodePoolSettings {
        fn default() -> Self {
            NodePoolSettings {
                name: "test-pool-1".to_string(),
                size: "vm-small".to_string(),
                count: 1,
                min_count: None,
                max_count: None,
                tags: None,
                labels: None,
                taints: None,
            }
        }
    }

    #[tokio::test]
    async fn when_spec_updated_update_node_pool() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| {
                Box::pin(async {
                    Ok(vec![Pod {
                        metadata: ObjectMeta {
                            labels: Some({
                                let mut map = BTreeMap::new();
                                map.insert(
                                    LABEL_MANAGED_NODE_POOL.to_string(),
                                    "pool1.pool1_namespace".to_string(),
                                );
                                map
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    }])
                })
            });

        client
            .expect_list_pods_with_fields()
            .with(eq("status.phase==Pending"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let mut original_settings: NodePoolSettings = Default::default();

        // Cloud provider can set it's own pool labels and tags
        // we should not take them into consideration when comparing settings
        original_settings
            .labels
            .get_or_insert(HashMap::new())
            .insert("external_label1".to_string(), "value1".to_string());

        let updated_settings = NodePoolSettings {
            name: "test-pool-1".to_string(),
            size: "vm-large".to_string(),
            count: 2,
            min_count: None,
            max_count: None,
            tags: Some(vec!["tag1".to_string()]),
            labels: Some({
                let mut map = HashMap::new();
                map.insert("label1".to_string(), "value1".to_string());
                map
            }),
            taints: Some(vec![NodePoolTaint {
                key: "taint1".to_string(),
                value: "value1".to_string(),
                effect: "NoSchedule".to_string(),
            }]),
        };

        // Update should reapply special taint and label
        let expected_settings = NodePoolSettings {
            name: "test-pool-1".to_string(),
            size: "vm-large".to_string(),
            count: 2,
            min_count: None,
            max_count: None,
            tags: Some(vec!["tag1".to_string()]),
            labels: Some({
                let mut map = HashMap::new();
                map.insert("label1".to_string(), "value1".to_string());
                map.insert(
                    LABEL_MANAGED_NODE_POOL.to_string(),
                    "pool1.pool1_namespace".to_string(),
                );
                map
            }),
            taints: Some(vec![
                NodePoolTaint {
                    key: "taint1".to_string(),
                    value: "value1".to_string(),
                    effect: "NoSchedule".to_string(),
                },
                NodePoolTaint {
                    key: LABEL_MANAGED_NODE_POOL.to_string(),
                    value: "pool1_uid".to_string(),
                    effect: "NoSchedule".to_string(),
                },
            ]),
        };

        let original_pool = NodePool {
            id: "test-pool-id".to_string(),
            settings: original_settings.clone(),
        };

        let updated_pool = NodePool {
            id: "test-pool-id".to_string(),
            settings: updated_settings.clone(),
        };

        cloud_provider
            .expect_find_pool_by_id()
            .with(eq(original_pool.id.clone()))
            .return_once(|_| Box::pin(async { Ok(Some(original_pool)) }));

        cloud_provider
            .expect_update_pool_by_id()
            .with(eq(updated_pool.id.clone()), eq(expected_settings.clone()))
            .return_once(|_, _| Box::pin(async { Ok(updated_pool) }))
            .once();

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: updated_settings,
                idle_timeout: None,
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_id: Some("test-pool-id".to_string()),
                ..Default::default()
            }),
        };
        let pool = Arc::new(pool);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_external_labels_updated_not_update_pool() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| {
                Box::pin(async {
                    Ok(vec![Pod {
                        metadata: ObjectMeta {
                            labels: Some({
                                let mut map = BTreeMap::new();
                                map.insert(
                                    LABEL_MANAGED_NODE_POOL.to_string(),
                                    "pool1.pool1_namespace".to_string(),
                                );
                                map
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    }])
                })
            });

        client
            .expect_list_pods_with_fields()
            .with(eq("status.phase==Pending"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let mut original_settings: NodePoolSettings = Default::default();
        let updated_settings = original_settings.clone();

        // Cloud provider can set it's own pool labels and tags
        // we should not take them into consideration when comparing settings
        original_settings
            .labels
            .get_or_insert(HashMap::new())
            .insert("label1".to_string(), "value1".to_string());

        original_settings
            .taints
            .get_or_insert(vec![])
            .push(NodePoolTaint {
                effect: "NoSchedule".to_string(),
                key: "taint1".to_string(),
                value: "value1".to_string(),
            });

        original_settings
            .tags
            .get_or_insert(vec![])
            .push("tag1".to_string());

        let original_pool = NodePool {
            id: "test-pool-id".to_string(),
            settings: original_settings.clone(),
        };

        cloud_provider
            .expect_find_pool_by_id()
            .with(eq(original_pool.id.clone()))
            .return_once(|_| Box::pin(async { Ok(Some(original_pool)) }));

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: updated_settings,
                idle_timeout: None,
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_id: Some("test-pool-id".to_string()),
                ..Default::default()
            }),
        };
        let pool = Arc::new(pool);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_spec_deleted_destroy_node_pool() {
        let client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        cloud_provider
            .expect_destroy_pool_by_id()
            .with(eq("test-pool-id"))
            .return_once(|_| Box::pin(async { Ok(true) }))
            .once();

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: Default::default(),
                idle_timeout: None,
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_id: Some("test-pool-id".to_string()),
                ..Default::default()
            }),
        };
        let pool = Arc::new(pool);

        let action = operator.cleanup(pool).await.unwrap();

        assert_eq!(action, Action::await_change());
    }

    #[tokio::test]
    async fn when_no_demand_do_nothing() {
        let mut client = MockKubeClient::new();
        let cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_list_pods_with_fields()
            .with(eq("status.phase==Pending"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                name: Some("pool1".to_string()),
                namespace: Some("namespace1".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "".to_string(),
                    size: "".to_string(),
                    count: 1,
                    min_count: None,
                    max_count: None,
                    tags: None,
                    labels: None,
                    taints: None,
                },
                idle_timeout: None,
            },
            status: None,
        };
        let pool = Arc::new(pool);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_pending_pods_dont_match_do_nothing() {
        let mut client = MockKubeClient::new();
        let cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let pending_pods = vec![
            Pod {
                metadata: ObjectMeta {
                    name: Some("pod1".to_string()),
                    namespace: Some("pod1_namespace".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
            Pod {
                metadata: ObjectMeta {
                    name: Some("pod2".to_string()),
                    namespace: Some("pod2_namespace".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];

        client
            .expect_list_pods_with_fields()
            .with(eq("status.phase==Pending"))
            .return_once(|_| Box::pin(async { Ok(pending_pods) }));

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                name: Some("pool1".to_string()),
                namespace: Some("namespace1".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "".to_string(),
                    size: "".to_string(),
                    count: 1,
                    min_count: None,
                    max_count: None,
                    tags: None,
                    labels: None,
                    taints: None,
                },
                idle_timeout: None,
            },
            status: None,
        };
        let pool = Arc::new(pool);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_pending_pods_match_patch_them_and_create_pool() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let pending_pods = vec![
            Pod {
                metadata: ObjectMeta {
                    name: Some("pod1".to_string()),
                    namespace: Some("pod1_namespace".to_string()),
                    ..Default::default()
                },
                ..Default::default()
            },
            Pod {
                metadata: ObjectMeta {
                    name: Some("pod2".to_string()),
                    namespace: Some("pod2_namespace".to_string()),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    node_selector: Some({
                        let mut map = BTreeMap::new();
                        map.insert(
                            LABEL_MANAGED_NODE_POOL.to_string(),
                            "pool1.pool1_namespace".to_string(),
                        );
                        map
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];

        client
            .expect_list_pods_with_fields()
            .with(eq(FIELD_SELECTOR_PENDING))
            .return_once(|_| Box::pin(async { Ok(pending_pods) }));

        client
            .expect_patch_pod()
            .withf(|pod, _| {
                pod.metadata.name.as_deref() == Some("pod2")
                    && pod.metadata.namespace.as_deref() == Some("pod2_namespace")
            })
            .return_once(|_, _| Box::pin(async { Ok(()) }))
            .once();

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "".to_string(),
                    size: "".to_string(),
                    count: 1,
                    min_count: None,
                    max_count: None,
                    tags: Some(vec!["tag1".to_string()]),
                    labels: Some({
                        let mut labels = HashMap::new();
                        labels.insert("label1".to_string(), "value1".to_string());
                        labels
                    }),
                    taints: Some(vec![NodePoolTaint {
                        effect: "NoSchedule".to_string(),
                        key: "taint1".to_string(),
                        value: "value1".to_string(),
                    }]),
                },
                idle_timeout: None,
            },
            status: None,
        };
        let pool = Arc::new(pool);

        let mut expected_pool_settings = pool.spec.settings.clone();
        expected_pool_settings.labels = expected_pool_settings
            .labels
            .or_else(|| Some(HashMap::new()));
        expected_pool_settings.labels.as_mut().unwrap().insert(
            LABEL_MANAGED_NODE_POOL.to_string(),
            "pool1.pool1_namespace".to_string(),
        );
        expected_pool_settings.taints = expected_pool_settings.taints.or_else(|| Some(vec![]));
        expected_pool_settings
            .taints
            .as_mut()
            .unwrap()
            .push(NodePoolTaint {
                key: LABEL_MANAGED_NODE_POOL.to_string(),
                value: "pool1_uid".to_string(),
                effect: "NoSchedule".to_string(),
            });

        cloud_provider
            .expect_create_pool()
            .withf(move |settings| settings == &expected_pool_settings)
            .return_once(move |settings| {
                let settings = settings.clone();
                Box::pin(async {
                    Ok(NodePool {
                        id: "node_pool_id".to_string(),
                        settings,
                    })
                })
            })
            .once();

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.is_none()
                    && status.node_pool_status == Some(NodePoolStatus::CREATING)
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.as_deref() == Some("node_pool_id")
                    && status.node_pool_status == Some(NodePoolStatus::CREATED)
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_labelled_pods_and_no_node_pool_create_node_pool() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| {
                Box::pin(async {
                    Ok(vec![Pod {
                        metadata: ObjectMeta {
                            name: Some("pod2".to_string()),
                            namespace: Some("pod2_namespace".to_string()),
                            ..Default::default()
                        },
                        spec: Some(PodSpec {
                            node_selector: Some({
                                let mut map = BTreeMap::new();
                                map.insert(
                                    LABEL_MANAGED_NODE_POOL.to_string(),
                                    "pool1.pool1_namespace".to_string(),
                                );
                                map
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }])
                })
            });

        client
            .expect_list_pods_with_fields()
            .with(eq(FIELD_SELECTOR_PENDING))
            .return_once(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_patch_pod()
            .withf(|pod, _| {
                pod.metadata.name.as_deref() == Some("pod2")
                    && pod.metadata.namespace.as_deref() == Some("pod2_namespace")
            })
            .return_once(|_, _| Box::pin(async { Ok(()) }))
            .once();

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: Default::default(),
                idle_timeout: None,
            },
            status: None,
        };
        let pool = Arc::new(pool);

        let mut expected_pool_settings = pool.spec.settings.clone();
        expected_pool_settings.labels = expected_pool_settings
            .labels
            .or_else(|| Some(HashMap::new()));
        expected_pool_settings.labels.as_mut().unwrap().insert(
            LABEL_MANAGED_NODE_POOL.to_string(),
            "pool1.pool1_namespace".to_string(),
        );
        expected_pool_settings.taints = expected_pool_settings.taints.or_else(|| Some(vec![]));
        expected_pool_settings
            .taints
            .as_mut()
            .unwrap()
            .push(NodePoolTaint {
                key: LABEL_MANAGED_NODE_POOL.to_string(),
                value: "pool1_uid".to_string(),
                effect: "NoSchedule".to_string(),
            });

        cloud_provider
            .expect_create_pool()
            .withf(move |settings| settings == &expected_pool_settings)
            .return_once(move |settings| {
                let settings = settings.clone();
                Box::pin(async {
                    Ok(NodePool {
                        id: "node_pool_id".to_string(),
                        settings,
                    })
                })
            })
            .once();

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.is_none()
                    && status.node_pool_status == Some(NodePoolStatus::CREATING)
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.as_deref() == Some("node_pool_id")
                    && status.node_pool_status == Some(NodePoolStatus::CREATED)
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_no_pool_id_recorded_and_status_is_creating_then_sync() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| {
                Box::pin(async {
                    Ok(vec![Pod {
                        metadata: ObjectMeta {
                            labels: Some({
                                let mut map = BTreeMap::new();
                                map.insert(
                                    LABEL_MANAGED_NODE_POOL.to_string(),
                                    "pool1.pool1_namespace".to_string(),
                                );
                                map
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    }])
                })
            });

        client
            .expect_list_pods_with_fields()
            .with(eq(FIELD_SELECTOR_PENDING))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "test-pool-1".to_string(),
                    ..Default::default()
                },
                idle_timeout: None,
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_status: Some(NodePoolStatus::CREATING),
                ..Default::default()
            }),
        };
        let pool = Arc::new(pool);

        cloud_provider
            .expect_find_pool_by_name()
            .with(eq("test-pool-1"))
            .return_once(move |_| {
                Box::pin(async {
                    Ok(Some(NodePool {
                        id: "node_pool_id".to_string(),
                        settings: Default::default(),
                    }))
                })
            });

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.as_deref() == Some("node_pool_id")
                    && status.node_pool_status == Some(NodePoolStatus::CREATED)
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn when_node_pool_is_idle_schedule_delete() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        cloud_provider
            .expect_find_pool_by_id()
            .with(eq("node_pool_id"))
            .return_once(move |_| {
                Box::pin(async {
                    Ok(Some(NodePool {
                        id: "node_pool_id".to_string(),
                        settings: Default::default(),
                    }))
                })
            });

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_list_pods_with_fields()
            .with(eq(FIELD_SELECTOR_PENDING))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.as_deref() == Some("node_pool_id")
                    && status.destroy_after.is_some()
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "test-pool-1".to_string(),
                    ..Default::default()
                },
                idle_timeout: Some(Duration::from_secs(10)),
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_id: Some("node_pool_id".to_string()),
                node_pool_status: Some(NodePoolStatus::CREATED),
                destroy_after: None,
            }),
        };

        let pool = Arc::new(pool);

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(10)))
    }

    #[tokio::test]
    async fn when_node_pool_is_idle_and_idle_timeout_expires_destroy_node_pool() {
        let mut client = MockKubeClient::new();
        let mut cloud_provider = MockCloudProvider::new();

        cloud_provider
            .expect_find_pool_by_id()
            .with(eq("node_pool_id"))
            .return_once(move |_| {
                Box::pin(async {
                    Ok(Some(NodePool {
                        id: "node_pool_id".to_string(),
                        settings: Default::default(),
                    }))
                })
            });

        cloud_provider
            .expect_destroy_pool_by_id()
            .with(eq("node_pool_id"))
            .return_once(move |_| Box::pin(async { Ok(true) }))
            .once();

        client
            .expect_list_pods_with_labels()
            .with(eq("dgolubets.github.io/managed-node-pool"))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_list_pods_with_fields()
            .with(eq(FIELD_SELECTOR_PENDING))
            .returning(|_| Box::pin(async { Ok(vec![]) }));

        client
            .expect_patch_pool_status()
            .withf(|pool, status, _| {
                pool.metadata.name.as_deref() == Some("pool1")
                    && pool.metadata.namespace.as_deref() == Some("pool1_namespace")
                    && status.node_pool_id.is_none()
                    && status.destroy_after.is_none()
            })
            .return_once(move |_, _, _| Box::pin(async { Ok(()) }))
            .once();

        let pool = ManagedNodePool {
            metadata: ObjectMeta {
                uid: Some("pool1_uid".to_string()),
                name: Some("pool1".to_string()),
                namespace: Some("pool1_namespace".to_string()),
                ..Default::default()
            },
            spec: ManagedNodePoolSpec {
                settings: NodePoolSettings {
                    name: "test-pool-1".to_string(),
                    ..Default::default()
                },
                idle_timeout: Some(Duration::from_secs(10)),
            },
            status: Some(ManagedNodePoolStatus {
                node_pool_id: Some("node_pool_id".to_string()),
                node_pool_status: Some(NodePoolStatus::CREATED),
                destroy_after: Some(SystemTime::now()),
            }),
        };

        let pool = Arc::new(pool);

        let operator = Operator {
            client: client.into(),
            cloud_provider: cloud_provider.into(),
        };
        let operator = Arc::new(operator);

        let action = operator.reconcile(pool).await.unwrap();

        assert_eq!(action, Action::requeue(Duration::from_secs(3600)))
    }
}
