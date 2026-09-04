# The passthrough real-cluster suite's fixtures (aws-passthrough.sh renders
# it): CRs on three buckets — the main one, an SSE-KMS one, a cross-region
# one — an ambient-identity CR, and pods that mount them. Placeholders:
# __B__ __BK__ __B2__ __REGION__ __KMSKEY__.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-rw, namespace: s3-tenants }
spec:
  bucket: __B__
  keyPrefix: pt/rw
  region: __REGION__
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-many, namespace: s3-tenants }
spec:
  bucket: __B__
  keyPrefix: pt/many
  region: __REGION__
  readOnly: true
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
---
# Ambient: the worker's own AWS chain — on EC2 the node's instance role.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-ambient, namespace: s3-tenants }
spec:
  bucket: __B__
  keyPrefix: pt/ambient
  region: __REGION__
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
  identity: { mode: ambient }
---
# SSE-KMS by bucket default: nothing on the mount names the key.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-kms, namespace: s3-tenants }
spec:
  bucket: __BK__
  keyPrefix: pt/kms
  region: __REGION__
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
---
# Cross-region: the bucket lives in us-east-1, the nodes in __REGION__.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-use1, namespace: s3-tenants }
spec:
  bucket: __B2__
  keyPrefix: pt/use1
  region: us-east-1
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
