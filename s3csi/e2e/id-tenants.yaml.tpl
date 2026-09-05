# Identity-mode legs (aws-identity.sh). Rendered: __B__ __ENDPOINT__ __REGION__.
---
# LEAN on the platform's own chain: nothing is injected, the syncer's
# AWS SDK runs its default provider chain.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-amb, namespace: s3-tenants }
spec:
  projectId: team-a/proj-amb
  bucket: __B__
  keyPrefix: tenants/amb
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  identity: { mode: ambient }
  consumers: { serviceAccounts: [trainer] }
---
# LEAN over the broker's STS facade: the syncer holds a projected token
# and calls AssumeRoleWithWebIdentity itself. Its client is the Rust AWS
# SDK, which is NOT the same client mount-s3 uses.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-wi, namespace: s3-tenants }
spec:
  projectId: team-a/proj-wi
  bucket: __B__
  keyPrefix: tenants/wi
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  identity: { mode: webIdentity }
  consumers: { serviceAccounts: [trainer] }
---
# PASSTHROUGH over the same facade. Its client is mount-s3's CRT, whose
# web-identity provider the design records as HTTPS-only — the claim
# this leg is here to check rather than repeat.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: pt-wi, namespace: s3-tenants }
spec:
  bucket: __B__
  keyPrefix: pt/wi
  region: __REGION__
  uid: 1001
  consumers: { serviceAccounts: [trainer] }
  identity: { mode: webIdentity }
