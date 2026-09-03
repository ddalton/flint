# One cluster's worth of the multi-cluster rig (run-multi.sh renders it
# per cluster with the shared MinIO's address). Everything a cluster
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
  AWS_ACCESS_KEY_ID: drill
  AWS_SECRET_ACCESS_KEY: drillsecret
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
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: datasets, namespace: s3-tenants }
spec:
  bucket: s3bucket
  keyPrefix: datasets/imagenet
  endpoint: http://__MINIO__:9000
  region: us-east-1
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: flint.io/v1alpha1
kind: FlintPassthroughMount
metadata: { name: elsewhere, namespace: s3-tenants }
spec:
  bucket: s3bucket
  keyPrefix: elsewhere
  endpoint: http://__MINIO__:9000
  region: us-east-1
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: flint.io/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj, namespace: s3-tenants }
spec:
  projectId: team-a/proj
  bucket: s3bucket
  keyPrefix: tenants/proj
  endpoint: http://__MINIO__:9000
  floorSecs: 3600
  expectedBytes: 8388608
  expectedFiles: 500
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
