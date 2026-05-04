#!/usr/bin/env bash
# Emit Kubernetes manifests for the pNFS cross-host bench. Stdout is a
# multi-document YAML stream the orchestrator pipes into `kubectl apply`.
# All values come from env vars so the script is idempotent and the
# orchestrator can re-render between sweep cells without writing temp
# files.
#
# Required env:
#   PNFS_IMAGE          container image (must exist in a registry the
#                       cluster can pull from).
#   MDS_NODE            kubernetes node name pinned for the MDS pod.
#   DS_NODES            space-separated node names (one DS per node).
#   CLIENT_NODE         kubernetes node name pinned for the client.
#   NAMESPACE           defaults to pnfs-bench.

set -uo pipefail

NS="${NAMESPACE:-pnfs-bench}"
IMG="${PNFS_IMAGE:?PNFS_IMAGE required}"
MDS_NODE="${MDS_NODE:?MDS_NODE required}"
CLIENT_NODE="${CLIENT_NODE:?CLIENT_NODE required}"
read -ra DS_NODE_ARR <<< "${DS_NODES:?DS_NODES required (space-separated)}"

cat <<EOF
---
apiVersion: v1
kind: Namespace
metadata:
  name: $NS
---
# MDS config — single fsid, stripe layout, memory backend (perf bench
# doesn't need restart survival; sqlite would just add fsync noise to
# the numbers we're trying to measure).
apiVersion: v1
kind: ConfigMap
metadata:
  name: pnfs-mds-config
  namespace: $NS
data:
  pnfs.yaml: |
    apiVersion: flint.io/v1alpha1
    kind: PnfsConfig
    mode: mds
    mds:
      bind: { address: "0.0.0.0", port: 2049 }
      layout:
        type: file
        stripeSize: 8388608
        policy: stripe
      dataServers: []   # DSes self-register over gRPC
      state: { backend: memory, config: {} }
      ha: { enabled: false, replicas: 1, leaderElection: false, leaseDuration: 15, renewDeadline: 10, retryPeriod: 2 }
      failover: { heartbeatTimeout: 30, policy: recall_affected, gracePeriod: 60 }
    exports:
      - path: /var/lib/flint-pnfs/exports
        fsid: 1
        options: [rw, sync, no_subtree_check]
        access:
          - network: 0.0.0.0/0
            permissions: rw
    logging: { level: info, format: text }
    monitoring:
      prometheus: { enabled: false, port: 0, path: /metrics }
      health: { enabled: false, port: 0, path: /health }
      metrics: []
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pnfs-mds
  namespace: $NS
spec:
  replicas: 1
  selector: { matchLabels: { app: pnfs-mds } }
  template:
    metadata: { labels: { app: pnfs-mds } }
    spec:
      nodeName: $MDS_NODE
      containers:
        - name: mds
          image: $IMG
          imagePullPolicy: IfNotPresent
          command: ["/usr/local/bin/flint-pnfs-mds"]
          args: ["--config", "/etc/flint/pnfs.yaml"]
          env:
            - { name: RUST_LOG, value: "info" }
            - { name: PNFS_INSTANCE_ID, value: "$(date +%s%N)" }
            - { name: PNFS_SERVER_SCOPE, value: "flint-pnfs-mds" }
          ports:
            - { containerPort: 2049, name: nfs }
            - { containerPort: 50051, name: grpc }
          volumeMounts:
            - { name: config, mountPath: /etc/flint }
            - { name: exports, mountPath: /var/lib/flint-pnfs/exports }
          securityContext: { privileged: true }
      volumes:
        - { name: config, configMap: { name: pnfs-mds-config } }
        - { name: exports, emptyDir: {} }
---
apiVersion: v1
kind: Service
metadata:
  name: pnfs-mds
  namespace: $NS
spec:
  selector: { app: pnfs-mds }
  ports:
    - { name: nfs,  port: 2049,  targetPort: 2049 }
    - { name: grpc, port: 50051, targetPort: 50051 }
EOF

# One DS Deployment per DS_NODE, each pinned to a specific worker.
i=0
for ds_node in "${DS_NODE_ARR[@]}"; do
  i=$((i+1))
  cat <<EOF
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: pnfs-ds${i}-config
  namespace: $NS
data:
  pnfs.yaml: |
    apiVersion: flint.io/v1alpha1
    kind: PnfsConfig
    mode: ds
    ds:
      bind: { address: "0.0.0.0", port: 2049 }
      deviceId: ds-${ds_node}
      mds:
        endpoint: pnfs-mds.$NS.svc.cluster.local:50051
        heartbeatInterval: 5
        registrationRetry: 2
        maxRetries: 0
      bdevs:
        - { name: lvol0, mount_point: /var/lib/flint-pnfs/exports }
      resources: { maxConnections: 1000, ioQueueDepth: 128, ioBufferSize: 1048576 }
      performance: { useSpdkIo: false, ioThreads: 4, zeroCopy: true }
    exports:
      - path: /var/lib/flint-pnfs/exports
        fsid: 1
        options: [rw, sync, no_subtree_check]
        access:
          - { network: 0.0.0.0/0, permissions: rw }
    logging: { level: info, format: text }
    monitoring:
      prometheus: { enabled: false, port: 0, path: /metrics }
      health: { enabled: false, port: 0, path: /health }
      metrics: []
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pnfs-ds${i}
  namespace: $NS
spec:
  replicas: 1
  selector: { matchLabels: { app: pnfs-ds, ds: ds${i} } }
  template:
    metadata: { labels: { app: pnfs-ds, ds: ds${i} } }
    spec:
      nodeName: ${ds_node}
      hostNetwork: true   # DS reports its bind address; hostNetwork
                          # gives a cluster-routable address out of the
                          # box without Service-level NodePort work.
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: ds
          image: $IMG
          imagePullPolicy: IfNotPresent
          command: ["/usr/local/bin/flint-pnfs-ds"]
          args: ["--config", "/etc/flint/pnfs.yaml"]
          env:
            - { name: PNFS_SERVER_SCOPE, value: "flint-pnfs-ds" }
            - name: POD_IP
              valueFrom:
                fieldRef:
                  fieldPath: status.podIP
          volumeMounts:
            - { name: config, mountPath: /etc/flint }
            - { name: data, mountPath: /var/lib/flint-pnfs/exports }
          securityContext: { privileged: true }
      volumes:
        - { name: config, configMap: { name: pnfs-ds${i}-config } }
        # Local-disk emptyDir keeps the bench measuring pNFS, not
        # network-attached PV layers. For a perf-tier production
        # deployment, swap to a hostPath on local NVMe.
        - { name: data, emptyDir: {} }
EOF
done

# Client harness pod — fio in a Deployment so we can `kubectl exec`
# multiple sweep cells against the same mount instead of paying
# pod-startup latency per fio run.
cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pnfs-bench-client
  namespace: $NS
spec:
  replicas: 1
  selector: { matchLabels: { app: pnfs-bench-client } }
  template:
    metadata: { labels: { app: pnfs-bench-client } }
    spec:
      nodeName: $CLIENT_NODE
      hostNetwork: true   # NFSv4.1 mount hits the MDS Service IP
                          # directly; hostNetwork sidesteps CNI hops
                          # the bench would otherwise be measuring.
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: client
          image: dilipdalton/flint-pnfs-bench-client:latest
          imagePullPolicy: Always
          command: ["sleep", "infinity"]
          securityContext:
            privileged: true   # mount(8) needs CAP_SYS_ADMIN
          readinessProbe:
            exec:
              command: ["test", "-x", "/usr/bin/fio"]
            periodSeconds: 5
          volumeMounts:
            - { name: results, mountPath: /results }
      volumes:
        - { name: results, emptyDir: {} }
EOF
