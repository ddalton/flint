#!/usr/bin/env bash
#
# Multi-client pNFS read scaling bench.
#
# Proves that pNFS read throughput scales with concurrent clients.
# Each client reads different files → layouts fan reads across DSes.
#
# Topology:
#   MDS:     1 dedicated node
#   DSes:    N nodes (data servers)
#   Clients: C pods colocated on DS nodes (no port conflict — clients
#            don't bind ports; DSes use hostNetwork:2049)
#
# Protocol:
#   1. Deploy MDS + DSes + C client pods
#   2. Each client writes a unique 1GB file (populates DSes)
#   3. Drop client caches
#   4. Run read bench: first 1 client alone, then all C clients simultaneously
#   5. Report aggregate throughput at each client count

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
HERE="$REPO_ROOT/tests/k8s/pnfs-bench"
RESULTS_DIR="$HERE/results"
mkdir -p "$RESULTS_DIR"

: "${KUBECONFIG:?KUBECONFIG must point at the cluster}"
: "${PNFS_IMAGE:?PNFS_IMAGE required}"
: "${MDS_NODE:?MDS_NODE required}"
: "${DS_NODES:?DS_NODES required (space-separated)}"
: "${CLIENT_NODES:?CLIENT_NODES required (space-separated node names for client pods)}"
NS="${NAMESPACE:-pnfs-bench}"
NCONNECT="${NCONNECT_OVERRIDE:-16}"
SIZE_MB="${SIZE_MB_OVERRIDE:-1024}"
BS="${BS_OVERRIDE:-1M}"
JOBS="${JOBS_OVERRIDE:-4}"

read -ra DS_NODE_ARR <<< "$DS_NODES"
read -ra CLIENT_NODE_ARR <<< "$CLIENT_NODES"
DS_COUNT=${#DS_NODE_ARR[@]}
CLIENT_COUNT=${#CLIENT_NODE_ARR[@]}
TS=$(date +%Y-%m-%d-%H%M%S)
LOG="$RESULTS_DIR/multi-client-$TS.log"
: > "$LOG"

log() { printf '%s  %s\n' "$(date +%H:%M:%S)" "$*" | tee -a "$LOG"; }

cleanup() {
  set +e
  log "▶ teardown: deleting namespace $NS"
  kubectl delete namespace "$NS" --wait=false --ignore-not-found=true >>"$LOG" 2>&1
}
trap cleanup EXIT

log "▶ Multi-client pNFS read scaling bench"
log "  MDS:     $MDS_NODE"
log "  DSes:    N=$DS_COUNT ($DS_NODES)"
log "  Clients: C=$CLIENT_COUNT ($CLIENT_NODES)"
log "  Config:  ${SIZE_MB}MB × bs=$BS × jobs=$JOBS per client"

# ─── Pre-flight ───────────────────────────────────────────────────────
if ! kubectl version --request-timeout=5s >/dev/null 2>&1; then
  log "✗ kubectl can't reach cluster"; exit 1
fi

# ─── Generate and apply manifests ─────────────────────────────────────
generate_manifests() {
  cat <<EOF
---
apiVersion: v1
kind: Namespace
metadata:
  name: $NS
---
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
      dataServers: []
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
          image: $PNFS_IMAGE
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

  # DS deployments
  local i=0
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
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: ds
          image: $PNFS_IMAGE
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
        - { name: data, emptyDir: {} }
EOF
  done

  # Client pods — one per CLIENT_NODE
  local c=0
  for client_node in "${CLIENT_NODE_ARR[@]}"; do
    c=$((c+1))
    cat <<EOF
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pnfs-client-${c}
  namespace: $NS
spec:
  replicas: 1
  selector: { matchLabels: { app: pnfs-client, client: client${c} } }
  template:
    metadata: { labels: { app: pnfs-client, client: client${c} } }
    spec:
      nodeName: ${client_node}
      hostNetwork: true
      dnsPolicy: ClusterFirstWithHostNet
      containers:
        - name: client
          image: dilipdalton/flint-pnfs-bench-client:latest
          imagePullPolicy: Always
          command: ["sleep", "infinity"]
          securityContext:
            privileged: true
          readinessProbe:
            exec:
              command: ["test", "-x", "/usr/bin/fio"]
            periodSeconds: 5
          volumeMounts:
            - { name: results, mountPath: /results }
      volumes:
        - { name: results, emptyDir: {} }
EOF
  done
}

log "▶ applying manifests"
generate_manifests | kubectl apply -f - >>"$LOG" 2>&1

# Wait for MDS
log "▶ waiting for MDS Ready"
kubectl -n "$NS" wait --for=condition=available deploy/pnfs-mds --timeout=120s >>"$LOG" 2>&1 || {
  log "✗ MDS didn't become Ready"; exit 1; }

# Wait for DSes
for i in $(seq 1 "$DS_COUNT"); do
  kubectl -n "$NS" wait --for=condition=available "deploy/pnfs-ds$i" --timeout=120s >>"$LOG" 2>&1 || {
    log "✗ DS$i didn't become Ready"; exit 1; }
done

# Wait for clients
for c in $(seq 1 "$CLIENT_COUNT"); do
  kubectl -n "$NS" wait --for=condition=available "deploy/pnfs-client-$c" --timeout=300s >>"$LOG" 2>&1 || {
    log "✗ client-$c didn't become Ready"; exit 1; }
done

# Give DSes time to register
log "▶ giving DSes 10s to register with MDS"
sleep 10
REGISTERED=$(kubectl -n "$NS" logs deploy/pnfs-mds | grep -c "DS registered successfully" || true)
log "  registered: $REGISTERED (expected: $DS_COUNT)"

# Get MDS Service IP
MDS_IP=$(kubectl -n "$NS" get svc pnfs-mds -o jsonpath='{.spec.clusterIP}')
log "▶ MDS Service ClusterIP: $MDS_IP"

# Get client pod names
declare -a CLIENT_PODS
for c in $(seq 1 "$CLIENT_COUNT"); do
  CLIENT_PODS[$c]=$(kubectl -n "$NS" get pod -l client=client${c} -o jsonpath='{.items[0].metadata.name}')
  log "  client-$c pod: ${CLIENT_PODS[$c]}"
done

# ─── Mount on all clients ─────────────────────────────────────────────
mount_cmd="set -eux; mountpoint -q /mnt/pnfs && umount -lf /mnt/pnfs || true; mkdir -p /mnt/pnfs; mount -t nfs4 -o minorversion=1,port=2049,nconnect=$NCONNECT,rsize=1048576,wsize=1048576 $MDS_IP:/ /mnt/pnfs; ls /mnt/pnfs"

for c in $(seq 1 "$CLIENT_COUNT"); do
  log "▶ mounting on client-$c"
  kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc "$mount_cmd" >>"$LOG" 2>&1 || {
    log "✗ mount failed on client-$c"; exit 1; }
done
log "✓ all clients mounted"

# ─── Write phase: each client writes a unique file ────────────────────
log "▶ write phase: each client writes ${SIZE_MB}MB to /mnt/pnfs/data-clientN"
for c in $(seq 1 "$CLIENT_COUNT"); do
  write_cmd="fio --name=write --filename=/mnt/pnfs/data-client${c} --rw=write --bs=$BS --numjobs=1 --size=${SIZE_MB}M --ioengine=libaio --iodepth=16 --direct=0 --end_fsync=1 --group_reporting --output-format=json > /results/write-c${c}.json 2>/dev/null"
  kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc "$write_cmd" >>"$LOG" 2>&1 &
done
wait
log "✓ write phase complete"

# ─── Drop caches on all clients ──────────────────────────────────────
for c in $(seq 1 "$CLIENT_COUNT"); do
  kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc 'echo 3 > /proc/sys/vm/drop_caches 2>/dev/null' >/dev/null 2>&1 || true
done
log "✓ caches dropped"

# ─── Read phase: measure at different client counts ───────────────────
log ""
log "════════════════════════════════════════════════════════════════"
log "READ SCALING RESULTS — $DS_COUNT DSes, $JOBS jobs/client, bs=$BS"
log "════════════════════════════════════════════════════════════════"
printf '\n| %-10s | %-12s | %-12s | %-8s |\n' "Clients" "Aggregate" "Per-client" "Scale"
printf '| %-10s | %-12s | %-12s | %-8s |\n' "----------" "------------" "------------" "--------"

BASELINE_MIBS=""

for num_clients in 1 2 3 4; do
  if [ "$num_clients" -gt "$CLIENT_COUNT" ]; then break; fi

  # Drop caches before each measurement
  for c in $(seq 1 "$num_clients"); do
    kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc 'echo 3 > /proc/sys/vm/drop_caches 2>/dev/null' >/dev/null 2>&1 || true
  done
  sleep 2

  # Launch fio on $num_clients simultaneously, each reading its own file
  declare -a READ_PIDS
  for c in $(seq 1 "$num_clients"); do
    read_cmd="fio --name=read --filename=/mnt/pnfs/data-client${c} --rw=read --bs=$BS --numjobs=$JOBS --size=${SIZE_MB}M --ioengine=libaio --iodepth=16 --direct=1 --group_reporting --output-format=json > /results/read-c${c}.json 2>/dev/null"
    kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc "$read_cmd" >>"$LOG" 2>&1 &
    READ_PIDS[$c]=$!
  done

  # Wait for all to finish
  for c in $(seq 1 "$num_clients"); do
    wait "${READ_PIDS[$c]}" 2>/dev/null || true
  done

  # Collect results
  total_kbps=0
  for c in $(seq 1 "$num_clients"); do
    kbps=$(kubectl -n "$NS" exec "${CLIENT_PODS[$c]}" -- bash -lc "jq -r '.jobs[0].read.bw // 0' /results/read-c${c}.json" 2>/dev/null | tr -d '\r')
    total_kbps=$((total_kbps + kbps))
  done

  agg_mibs=$(awk -v k="$total_kbps" 'BEGIN { printf "%.1f", k/1024 }')
  per_client_mibs=$(awk -v k="$total_kbps" -v n="$num_clients" 'BEGIN { printf "%.1f", k/1024/n }')

  if [ -z "$BASELINE_MIBS" ]; then
    BASELINE_MIBS="$agg_mibs"
    scale="1.00×"
  else
    scale=$(awk -v a="$agg_mibs" -v b="$BASELINE_MIBS" 'BEGIN { printf "%.2f×", a/b }')
  fi

  printf '| %-10s | %8s MiB/s | %8s MiB/s | %-8s |\n' "$num_clients" "$agg_mibs" "$per_client_mibs" "$scale"
  log "  $num_clients clients: aggregate=$agg_mibs MiB/s, per-client=$per_client_mibs MiB/s, scale=$scale"

  unset READ_PIDS
done

printf '\n'
log ""
log "▶ Log: $LOG"
exit 0
