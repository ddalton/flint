# The access drill's tenants (aws-access.sh). Rendered: __B__ __ENDPOINT__
# __REGION__. Three ServiceAccounts: `editor` (read-write where listed),
# `viewer` (read-only where listed) and `stranger` (listed nowhere).
#
# floorSecs 15 on the lean workspaces: a reader integrates at every floor,
# so the convergence legs wait tens of seconds, not an hour. Writers
# publish only when an agent declares it, because nothing in these pods
# writes on its own.
apiVersion: v1
kind: ServiceAccount
metadata: { name: editor, namespace: s3-tenants }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: viewer, namespace: s3-tenants }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: stranger, namespace: s3-tenants }
---
# One workspace, writers and readers at once: editor writes, viewer reads,
# and an editor pod with csi.readOnly: true reads too.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: acc-lean, namespace: s3-tenants }
spec:
  projectId: team-a/acc-lean
  bucket: __B__
  keyPrefix: access/lean
  endpoint: __ENDPOINT__
  region: __REGION__
  floorSecs: 15
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers:
    serviceAccounts: [editor]
    readOnlyServiceAccounts: [viewer]
---
# The same lists on a passthrough mount: mount-s3 under a read grant.
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: acc-pt, namespace: s3-tenants }
spec:
  bucket: __B__
  keyPrefix: access/pt
  region: __REGION__
  uid: 1001
  gid: 1001
  consumers:
    serviceAccounts: [editor]
    readOnlyServiceAccounts: [viewer]
---
# Precedence: "*" writes, but a NAMED read-only entry beats it.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: acc-wild, namespace: s3-tenants }
spec:
  projectId: team-a/acc-wild
  bucket: __B__
  keyPrefix: access/wild
  endpoint: __ENDPOINT__
  region: __REGION__
  floorSecs: 15
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers:
    serviceAccounts: ["*"]
    readOnlyServiceAccounts: [viewer]
---
# Narrowed while a writer is running: the leg moves editor from the
# read-write list to the read-only one and waits for the next mint.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: acc-narrow, namespace: s3-tenants }
spec:
  projectId: team-a/acc-narrow
  bucket: __B__
  keyPrefix: access/narrow
  endpoint: __ENDPOINT__
  region: __REGION__
  floorSecs: 15
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers:
    serviceAccounts: [editor]
---
# No ceiling: sizeLimitGib 0 makes the tree a plain directory on the node's
# root filesystem, with no loop-mounted image under it — the other layout a
# read-only bind has to hold on.
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: acc-direct, namespace: s3-tenants }
spec:
  projectId: team-a/acc-direct
  bucket: __B__
  keyPrefix: access/direct
  endpoint: __ENDPOINT__
  region: __REGION__
  floorSecs: 15
  sizeLimitGib: 0
  expectedBytes: 1048576
  expectedFiles: 100
  maxFiles: 5000
  uid: 1001
  gid: 1001
  consumers:
    serviceAccounts: [editor]
    readOnlyServiceAccounts: [viewer]
