#!/usr/bin/env bash
#
# ds-reschedule.sh — Phase 4 drill 1: DS pod reschedule under load.
#
# Cordon the target DS's node and delete its pod mid-write: the
# StatefulSet reschedules it to another node, the PVC follows over
# NVMe-oF (NodeStage self-heals a dead target if needed), and the
# per-pod ClusterIP is unchanged — so the kernel client's cached
# device info stays valid. Asserts: cross-node reschedule, same
# ClusterIP, same identity marker (no volume-swap WARN), writer
# finishes with zero errors, checksums clean. Reports the DS outage
# window and the max client-visible stall.
#
# NOTE: while the DS is down its per-pod Service has no endpoints, so
# client I/O touching its stripes falls back to READ/WRITE-through-MDS
# and gets stub-IO-guard DELAY until the DS returns. In-flight RPCs
# re-drive when the pod is back (bounded stall); a writer that never
# recovers is the known in-flight-wedge residual — this drill exists
# to measure which of the two happens under real reschedule timing.
#
#   KUBECONFIG=... CLIENT_NODE=<node> [TARGET_DS=flint-pnfs-ds-1] \
#     tests/k8s/pnfs-drills/ds-reschedule.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

TARGET_DS=${TARGET_DS:-flint-pnfs-ds-1}
WRITER=pnfs-d1-writer
CORDONED=""
cleanup() {
  [ -n "$CORDONED" ] && kubectl uncordon "$CORDONED" >/dev/null 2>&1
  cleanup_writer "$WRITER"
}
trap cleanup EXIT

need_env
step "preflight"
fleet_healthy
DS_NODE=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.nodeName}')
DS_IP=$(kubectl get svc -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.clusterIP}')
[ "$DS_NODE" != "$CLIENT_NODE" ] || fail "TARGET_DS runs on CLIENT_NODE — pick another target or client"
STAMP0=$(kubectl exec -n "$NS" "$TARGET_DS" -- sh -c 'grep created_at /data/.flint-ds-identity' 2>/dev/null)
ok "target ${TARGET_DS} on ${DS_NODE} (ClusterIP ${DS_IP}, ${STAMP0:-no marker})"

step "writer + load"
make_writer "$WRITER"
start_load "$WRITER"
sleep 8

step "cordon ${DS_NODE} + delete ${TARGET_DS}"
kubectl cordon "$DS_NODE" >/dev/null && CORDONED="$DS_NODE"
OLD_UID=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.metadata.uid}')
T0=$(date +%s)
kubectl delete pod -n "$NS" "$TARGET_DS" --wait=false >/dev/null
wait_pod_replaced "$NS" "$TARGET_DS" "$OLD_UID" 420 \
  || fail "${TARGET_DS} replacement did not become Ready"
T1=$(date +%s)
NEW_NODE=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.nodeName}')
NEW_IP=$(kubectl get svc -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.clusterIP}')
[ "$NEW_NODE" != "$DS_NODE" ] || fail "pod did not move nodes (cordon ignored?)"
[ "$NEW_IP" = "$DS_IP" ] || fail "per-pod ClusterIP changed: ${DS_IP} → ${NEW_IP}"
kubectl uncordon "$DS_NODE" >/dev/null && CORDONED=""
ok "rescheduled ${DS_NODE} → ${NEW_NODE} in $(( T1 - T0 ))s, ClusterIP stable"

step "identity + registration"
STAMP1=$(kubectl exec -n "$NS" "$TARGET_DS" -- sh -c 'grep created_at /data/.flint-ds-identity' 2>/dev/null)
[ "$STAMP1" = "$STAMP0" ] || fail "identity stamp changed across reschedule: '${STAMP0}' → '${STAMP1}'"
MDS_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds -o jsonpath='{.items[0].metadata.name}')
kubectl logs -n "$NS" "$MDS_POD" --since=10m | grep "DIFFERENT data volume" >/dev/null \
  && fail "MDS flagged a volume swap — PVC did not follow the pod"
ok "same volume followed the pod (stamp stable, no swap WARN)"

step "writer verdict (generous budget — measuring stall vs wedge)"
wait_load "$WRITER" 600
[ "$LOAD_STATUS" = "OK" ] || fail "writer status: ${LOAD_STATUS} — in-flight-wedge residual hit (see plan doc; fix = MDS proxy I/O)"
STALL=$(max_stall "$WRITER")
verify_load "$WRITER"
ok "writer OK; DS outage $(( T1 - T0 ))s, max client stall ${STALL}s"

printf '\n✅ PASS: DS reschedule under load (outage %ss, stall %ss, zero errors)\n' "$(( T1 - T0 ))" "$STALL"
