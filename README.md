# Managed node pool operator for K8s

![Test](https://github.com/DGolubets/k8s-managed-node-pool/actions/workflows/test.yaml/badge.svg)

Supported clouds:

- Digital Ocean

## About

This operator dynamically creates a node pool in a managed K8s when there are pending pods assigned to it and tears it down when there are no more running pods.

## Rationale

Suppose you want to run Spark jobs in your K8s cluster from time to time.
You'll likely need a dedicated pool of powerful machines for that.
They cost much more so you don't want to keep it running all the time.

First solution that comes to mind: just set auto scaling.
That should work just fine in Google Cloud, but not in Digital Ocean.
The latter doesn't support auto scaling node pools to 0 nodes (yet?).
So you endup with at least one node running all the time.

Another solution: set up and tear down a node pool programmatically when your application starts and stops.
E.g. your Spark driver application doesn't have to run on powerful machine itself and can take care of that.
The problem with that is if you deploy a little fix for your spark app you'll have to wait for it to tear down and set the pool up again, because your app doesn't know if you plan to restart it.

To solve that we need some background process that can actually wait for demand for the pool and take care of creation and destruction of it in efficient way.
And that sounds like a job for K8s Operator.

## Installation

```bash
helm install mnp oci://ghcr.io/dgolubets/managed-node-pool-operator-do \
--namespace managed-node-pool \
--create-namespace \
--set digitalOcean.clusterId='{cluster id}' \
--set digitalOcean.token='{token}'
```

Alternatively, to use custom secrets:

```bash
helm install mnp oci://ghcr.io/dgolubets/managed-node-pool-operator-do \
--namespace managed-node-pool \
--create-namespace \
--set digitalOcean.createSecret=false \
--set digitalOcean.clusterIdSecret.name='{cluster id secret name}' \
--set digitalOcean.clusterIdSecret.key='{cluster id secret key}' \
--set digitalOcean.tokenSecret.name='{token secret name}' \
--set digitalOcean.tokenSecret.key='{token secret key}'
```

## Example

Node pool spec:

```yaml
apiVersion: dgolubets.github.io/v1alpha1
kind: ManagedNodePool
metadata:
  name: managed-pool-1
  namespace: test
spec:
  name: managed-pool-test-1
  size: s-1vcpu-1gb
  count: 4
  min_count: 1
  max_count: 10
  labels:
    label1: "value1"
    label2: "value2"
  tags:
    - tag1
    - tag2
  idle_timeout: "15s"
```

Pod spec:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: test
  labels:
    app: test
spec:
  containers:
    - name: ubuntu
      image: ubuntu:22.04
      resources:
        requests:
          cpu: "100m"
          memory: "100Mi"
        limits:
          cpu: "100m"
          memory: "100Mi"
      command: ["/bin/bash", "-c", "--"]
      args: ["while true; do sleep 30; done;"]
  nodeSelector:
    dgolubets.github.io/managed-node-pool: managed-pool-1.test
```

The only requirement for pods is to have `nodeSelector` set, pointing to the pool they have to run on.
