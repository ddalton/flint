# Lean workspaces for aws-hardening.sh (rendered: __B__ __REGION__).
# The endpoint is the store: MinIO on kind, https://s3.<region>.amazonaws.com
# for STORE=s3. FlintLeanWorkspace has no spec.region — naming one is a
# strict-decoding BadRequest, which `apply` reports on stderr while the
# suite carries on into legs whose workspace does not exist (first run).
#
# floorSecs is an hour on every one: nothing here publishes on a timer,
# so every publish a leg observes was DECLARED by its pod or DRAINED at
# its delete — a reboot or a lost node can only be judged against
# publishes the drill made on purpose.
---
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-reboot, namespace: s3-tenants }
spec:
  projectId: team-a/proj-reboot
  bucket: __B__
  keyPrefix: tenants/reboot
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 8388608
  expectedFiles: 500
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-loss, namespace: s3-tenants }
spec:
  projectId: team-a/proj-loss
  bucket: __B__
  keyPrefix: tenants/loss
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 8388608
  expectedFiles: 500
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
# The big tree: __BIGN__ small files. maxFiles is explicit so the
# ceiling is visible next to the count it must exceed.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-big, namespace: s3-tenants }
spec:
  projectId: team-a/proj-big
  bucket: __B__
  keyPrefix: tenants/big
  endpoint: __ENDPOINT__
  floorSecs: 3600
  expectedBytes: 67108864
  expectedFiles: __BIGN__
  maxFiles: 250000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
---
# A one-GiB workspace: the image is mkfs.ext4 at the default inode
# ratio, so its ceiling on FILES arrives long before its ceiling on bytes.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: proj-tiny, namespace: s3-tenants }
spec:
  projectId: team-a/proj-tiny
  bucket: __B__
  keyPrefix: tenants/tiny
  endpoint: __ENDPOINT__
  floorSecs: 3600
  sizeLimitGib: 1
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 250000
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [trainer] }
