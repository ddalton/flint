# One cluster's worth of the multi-cluster rig (run-multi.sh renders it
# per cluster with the shared store's endpoint, bucket, region and key —
# the rig's MinIO on the docker network, or a real bucket with STORE=s3). Everything a cluster
# needs besides the chart: the broker's static Secret, a RESTRICTED
# tenant namespace with the consumer SA, and the CRs — the SAME
# bucket/prefix on every cluster, because that is the use case: agents
# on different clusters working the same project's artifacts through
# one S3 endpoint outside both.
---
apiVersion: v1
kind: Namespace
metadata: { name: flint-system }
---
apiVersion: v1
kind: Secret
metadata: { name: s3-broker-static, namespace: flint-system }
type: Opaque
stringData:
  AWS_ACCESS_KEY_ID: __KEY__
  AWS_SECRET_ACCESS_KEY: __SECRET__
---
# A durable mc pod in cluster 1, so the legs read the bucket through the
# same key the workers use (with STORE=s3 the Mac's docker cannot be
# handed the key on a URL: a secret with a `/` in it breaks the parse).
apiVersion: v1
kind: Pod
metadata: { name: mc-s3, namespace: flint-system }
spec:
  containers:
    - name: mc
      image: minio/mc
      command: ["/bin/sh", "-c"]
      args:
        - |
          trap 'exit 0' TERM INT
          until mc alias set m __ENDPOINT__ __KEY__ __SECRET__; do sleep 2; done
          sleep 86400 & wait
---
apiVersion: v1
kind: Namespace
metadata:
  name: s3-tenants
  labels:
    pod-security.kubernetes.io/enforce: restricted
    pod-security.kubernetes.io/enforce-version: latest
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: trainer, namespace: s3-tenants }
---
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: datasets, namespace: s3-tenants }
spec:
  bucket: __BUCKET__
  keyPrefix: datasets/imagenet
  endpoint: __ENDPOINT__
  region: __REGION__
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: elsewhere, namespace: s3-tenants }
spec:
  bucket: __BUCKET__
  keyPrefix: elsewhere
  endpoint: __ENDPOINT__
  region: __REGION__
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj, namespace: s3-tenants }
spec:
  projectId: team-a/proj
  bucket: __BUCKET__
  keyPrefix: tenants/proj
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 8388608
  expectedFiles: 500
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
