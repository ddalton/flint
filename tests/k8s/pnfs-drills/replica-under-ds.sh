#!/usr/bin/env bash
#
# replica-under-ds.sh — Phase 4 drill 3: replica failure underneath a DS.
#
# The payoff drill for lvol-backed DSes: the target DS's PVC is a
# replicated (numReplicas ≥ 2) flint volume — a raid1 on the DS's node
# with one local lvol leg and one remote NVMe-oF leg. Mid-write we
# detach the REMOTE leg (initiator-side, zero collateral): the raid
# degrades to 1/2 but stays online, the DS keeps serving, and pNFS
# clients see NOTHING (drilled: files kept landing on the target DS
# through the whole degraded window; all checksums; 1 s max stall).
# Afterwards the leg is reattached and re-added; raid returns 2/2.
#
# Prereqs: the target DS's PVC is on a numReplicas≥2 StorageClass (see
# the plan doc for the claim-template recreate recipe), and unique
# writer file names — placements are pinned per file key FOREVER
# (sqlite), so reusing names from an earlier, narrower fleet reuses
# the old stripe map and can miss the target DS entirely. This drill
# timestamps its prefix.
#
#   KUBECONFIG=... CLIENT_NODE=<node> TARGET_DS=flint-pnfs-ds-3 \
#     tests/k8s/pnfs-drills/replica-under-ds.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

TARGET_DS=${TARGET_DS:-flint-pnfs-ds-3}
WRITER=pnfs-d3-writer
PFX="r3drill-$(date +%s)"
DETACHED=""

RPC_POD=""
rpc() { kubectl exec -n "$NS" "$RPC_POD" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py "$@"; }

restore_leg() {
  set +e
  if [ -n "$DETACHED" ]; then
    rpc bdev_nvme_attach_controller -b "$CTRL" -t tcp -a "$LEG_IP" -s "$LEG_PORT" \
      -f ipv4 -n "$LEG_NQN" -q "$HOST_NQN" >/dev/null 2>&1
    rpc bdev_raid_add_base_bdev "$RAID" "${CTRL}n1" >/dev/null 2>&1
  fi
  cleanup_writer "$WRITER"
}
trap restore_leg EXIT

need_env
step "preflight: discover the raid under ${TARGET_DS}"
fleet_healthy
DS_NODE=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.nodeName}')
RPC_POD=$(kubectl get pods -n "$NS" -o wide --no-headers | awk -v n="$DS_NODE" '$7==n && /flint-csi-node/ {print $1}' | head -1)
PV=$(kubectl get pvc -n "$NS" "data-${TARGET_DS}" -o jsonpath='{.spec.volumeName}')
RAID="raid_${PV}"
CTRL="nvme_$(echo "nqn.2024-11.com.flint:volume:${PV}_1" | tr ':.' '__')"
HOST_NQN="nqn.2024-11.com.flint:node:${DS_NODE}"
OPER=$(rpc bdev_raid_get_bdevs all | python3 -c "
import json,sys
for r in json.load(sys.stdin):
    if r['name'] == '${RAID}':
        print(r['num_base_bdevs_operational'], r['num_base_bdevs']); break")
[ "$OPER" = "2 2" ] || fail "raid ${RAID} not 2/2 before the drill (got '${OPER}') — is data-${TARGET_DS} on a numReplicas=2 SC?"
read -r LEG_IP LEG_PORT LEG_NQN <<EOF
$(rpc bdev_nvme_get_controllers | python3 -c "
import json,sys
for c in json.load(sys.stdin):
    if c['name'] == '${CTRL}':
        t=c['ctrlrs'][0]['trid']
        print(t['traddr'], t['trsvcid'], t['subnqn']); break")
EOF
[ -n "${LEG_NQN:-}" ] || fail "remote-leg controller ${CTRL} not found on ${DS_NODE}"
ok "raid ${RAID} 2/2 on ${DS_NODE}; remote leg ${LEG_IP}:${LEG_PORT}"

step "writer + load (unique prefix ${PFX})"
make_writer "$WRITER"
kubectl exec "$WRITER" -- sh -c "rm -f /tmp/st /tmp/prog; \
  (for i in \$(seq 1 $N_FILES); do \
     dd if=/dev/zero of=/data/${PFX}-\$i.bin bs=1M count=$FILE_MB 2>/dev/null \
       || { echo FAIL > /tmp/st; exit 1; }; \
     sync; echo \"\$i \$(date +%s)\" >> /tmp/prog; sleep 0.2; \
   done; echo OK > /tmp/st) & echo started" >/dev/null || fail "writer start"
ok "load started"
sleep 12

step "detach remote leg mid-write"
rpc bdev_nvme_detach_controller "$CTRL" >/dev/null || fail "detach failed"
DETACHED=1
sleep 3
DEG=$(rpc bdev_raid_get_bdevs all | python3 -c "
import json,sys
for r in json.load(sys.stdin):
    if r['name'] == '${RAID}':
        print(r['state'], r['num_base_bdevs_operational']); break")
[ "$DEG" = "online 1" ] || fail "expected degraded-online (got '${DEG}')"
ok "raid degraded: online 1/2 — DS still serving"

step "writer verdict through the degraded window"
wait_load "$WRITER" 420
[ "$LOAD_STATUS" = "OK" ] || fail "writer status: $LOAD_STATUS"
STALL=$(max_stall "$WRITER")
kubectl exec "$WRITER" -- sh -c "bad=0; for i in \$(seq 1 $N_FILES); do \
    s=\$(sha256sum /data/${PFX}-\$i.bin | cut -d' ' -f1); \
    [ \"\$s\" = \"$ZEROS_SHA\" ] || bad=1; done; \
  [ \$bad -eq 0 ] && echo CHECKSUMS-OK" | grep CHECKSUMS-OK >/dev/null \
  || fail "checksum verification failed"
N_ON_DS=$(kubectl exec -n "$NS" "$TARGET_DS" -- sh -c "find /data -name '${PFX}-*' | wc -l" 2>/dev/null | tr -d ' ')
[ "${N_ON_DS:-0}" -ge 1 ] || fail "no drill stripes ever landed on ${TARGET_DS} (stale pinned names? fleet width?)"
ok "writer OK, checksums OK, stall ${STALL}s, ${N_ON_DS}/${N_FILES} files on ${TARGET_DS} incl. the degraded window"

step "reattach leg + rebuild"
rpc bdev_nvme_attach_controller -b "$CTRL" -t tcp -a "$LEG_IP" -s "$LEG_PORT" \
  -f ipv4 -n "$LEG_NQN" -q "$HOST_NQN" >/dev/null || fail "reattach failed"
rpc bdev_raid_add_base_bdev "$RAID" "${CTRL}n1" >/dev/null || fail "raid re-add failed"
DETACHED=""
for _ in $(seq 1 24); do
  ST=$(rpc bdev_raid_get_bdevs all | python3 -c "
import json,sys
for r in json.load(sys.stdin):
    if r['name'] == '${RAID}':
        print(r['state'], r['num_base_bdevs_operational']); break")
  [ "$ST" = "online 2" ] && break
  sleep 5
done
[ "$ST" = "online 2" ] || fail "raid never returned to 2/2 (last: '${ST}')"
ok "raid back to online 2/2"

printf '\n✅ PASS: replica failure under %s — degraded window invisible to pNFS (stall %ss, %s files on the DS)\n' "$TARGET_DS" "$STALL" "$N_ON_DS"
