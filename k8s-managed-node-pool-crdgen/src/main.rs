use k8s_managed_node_pool::model::ManagedNodePool;
use kube::CustomResourceExt;

fn main() {
    print!(
        "{}",
        serde_yaml::to_string(&ManagedNodePool::crd()).unwrap()
    )
}
