#!/usr/bin/env bash
# spdk-rwo-basic — the simplest SPDK CSI flow, end-to-end.
#
# Asserts that:
#   1. CreateVolume succeeds (PVC binds).
#   2. The PV carries the volume_context keys the inventory §6 lists for
#      single-replica RWO volumes.
#   3. A pod can mount the volume, write data, and read it back.
#   4. After pod restart, the data is still there (volume is durable
#      across pod lifecycle).
#   5. Cleanup leaves no orphan PV.
#
# What this protects: the most common Flint user flow — "a pod with a 1Gi
# PVC". If this scenario regresses after a refactor, it doesn't matter how
# pretty the code is, the driver is broken.
#
# Run:
#   tests/regression/spdk-rwo-basic.sh                # check against baseline
#   REGRESSION_RECORD=1 tests/regression/spdk-rwo-basic.sh   # capture baseline
#
# Requires:
#   - kubectl, jq, sha256sum on PATH
#   - KUBECONFIG points at a cluster with the Flint CSI driver installed
#   - StorageClass `flint` exists (or override REGRESSION_STORAGE_CLASS)

set -euo pipefail

source "$(dirname "$0")/lib/common.sh"

trap regression::teardown EXIT
regression::setup "spdk-rwo-basic"

PVC=spdk-rwo-basic-pvc
POD_A=writer
POD_B=reader

# ---------------------------------------------------------------------------
# Step 1 — provision the PVC.
# ---------------------------------------------------------------------------

regression::step "creating 1Gi RWO PVC via StorageClass $REGRESSION_STORAGE_CLASS"

regression::apply <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: $PVC
  namespace: @@NS@@
  labels:
    regression-scenario: spdk-rwo-basic
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 1Gi
  storageClassName: @@SC@@
EOF

# WaitForFirstConsumer is the default binding mode in the helm chart's
# values.yaml — meaning the PVC stays Pending until a pod uses it.
# Schedule the writer pod first; the PVC binds during pod creation.

# ---------------------------------------------------------------------------
# Step 2 — schedule a pod that writes a known payload.
# ---------------------------------------------------------------------------

regression::step "scheduling writer pod"

regression::apply <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $POD_A
  namespace: @@NS@@
  labels:
    regression-scenario: spdk-rwo-basic
spec:
  restartPolicy: Never
  containers:
  - name: writer
    image: busybox:1.36
    command: ["sh","-c"]
    args:
    - |
      set -eu
      mkdir -p /data
      # Write a deterministic payload — 4 MiB of /dev/urandom, hashed.
      # We need /dev/urandom because zeros would be indistinguishable
      # from a fresh-allocated thin volume.
      dd if=/dev/urandom of=/data/payload bs=1M count=4 2>&1
      sha256sum /data/payload | tee /data/payload.sha256
      sync
      # Hold the pod alive long enough for kubectl wait to see Ready.
      sleep 30
    volumeMounts:
    - name: vol
      mountPath: /data
  volumes:
  - name: vol
    persistentVolumeClaim:
      claimName: $PVC
EOF

regression::wait_pvc_bound "$PVC"
regression::wait_pod_ready "$POD_A"
regression::ok "writer pod Ready, PVC bound"

# ---------------------------------------------------------------------------
# Step 3 — assert PV contract from inventory §6.
# ---------------------------------------------------------------------------

regression::step "checking PV volume_context keys (inventory §6)"

PV=$(regression::pv_for_pvc "$PVC")
[ -n "$PV" ] || regression::fail "PVC bound but no PV name"
regression::log "PV name: $PV"

# These are the four keys the inventory says CreateVolume writes for a
# single-replica RWO volume (main.rs:841–865).
regression::assert_pv_attribute "$PV" "size"
regression::assert_pv_attribute "$PV" "flint.csi.storage.io/replica-count" "1"
regression::assert_pv_attribute "$PV" "flint.csi.storage.io/node-name"
regression::assert_pv_attribute "$PV" "flint.csi.storage.io/lvol-uuid"
regression::assert_pv_attribute "$PV" "flint.csi.storage.io/lvs-name"

# Snapshot the full attribute key set for baseline-drift detection. If a
# refactor adds or removes a key, this asserts the change is intentional
# (because someone has to update the baseline).
KEYS=$(kubectl get pv "$PV" -o json \
    | jq -r '.spec.csi.volumeAttributes | keys[]' | sort | tr '\n' ',' | sed 's/,$//')
regression::baseline "pv-attribute-keys" "$KEYS"

regression::ok "PV contract OK"

# ---------------------------------------------------------------------------
# Step 4 — capture the writer's hash, then delete writer.
# ---------------------------------------------------------------------------

regression::step "capturing payload hash from writer"

WRITER_HASH=$(regression::pod_exec "$POD_A" sh -c 'cat /data/payload.sha256 | awk "{print \$1}"')
[ -n "$WRITER_HASH" ] || regression::fail "writer never wrote payload.sha256"
regression::log "writer hash: $WRITER_HASH"

kubectl delete pod "$POD_A" -n "$REGRESSION_NAMESPACE" --wait=true \
    --timeout="${REGRESSION_TIMEOUT}s" >>"$SCENARIO_LOG" 2>&1
regression::wait_pod_gone "$POD_A"
regression::ok "writer terminated"

# ---------------------------------------------------------------------------
# Step 5 — schedule a fresh pod, read back, verify hash matches.
#
# This is the core durability assertion: the bytes a pod wrote survive
# across the pod's lifetime. If this fails, the volume isn't actually
# persistent (the PVC may be backed by emptyDir incorrectly, or the
# block device isn't being staged consistently across mount cycles).
# ---------------------------------------------------------------------------

regression::step "scheduling reader pod against same PVC"

regression::apply <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $POD_B
  namespace: @@NS@@
  labels:
    regression-scenario: spdk-rwo-basic
spec:
  restartPolicy: Never
  containers:
  - name: reader
    image: busybox:1.36
    command: ["sh","-c"]
    args:
    - |
      set -eu
      ls -la /data >&2
      sha256sum /data/payload
      sleep 30
    volumeMounts:
    - name: vol
      mountPath: /data
  volumes:
  - name: vol
    persistentVolumeClaim:
      claimName: $PVC
EOF

regression::wait_pod_ready "$POD_B"
READER_HASH=$(regression::pod_exec "$POD_B" sh -c 'sha256sum /data/payload | awk "{print \$1}"')

if [ "$READER_HASH" != "$WRITER_HASH" ]; then
    regression::fail "data corruption: writer wrote $WRITER_HASH, reader saw $READER_HASH"
fi
regression::ok "round-trip hash matches: $READER_HASH"

# ---------------------------------------------------------------------------
# Step 6 — clean up. Teardown verifies no orphan PV.
# ---------------------------------------------------------------------------

regression::step "cleanup"

kubectl delete pod "$POD_B" -n "$REGRESSION_NAMESPACE" --wait=true \
    --timeout="${REGRESSION_TIMEOUT}s" >>"$SCENARIO_LOG" 2>&1
kubectl delete pvc "$PVC" -n "$REGRESSION_NAMESPACE" --wait=true \
    --timeout="${REGRESSION_TIMEOUT}s" >>"$SCENARIO_LOG" 2>&1

regression::ok "spdk-rwo-basic PASS"
