#!/usr/bin/env bash
#
# mds-roll.sh — Phase 4 drill 4: MDS pod roll mid-workload.
#
# `kubectl rollout restart` of the MDS Deployment (Recreate strategy:
# clean unstage → fresh pod → sqlite reload → 90s grace) while a
# client writes. Asserts: rollout completes, placements reload, DSes
# re-register within one heartbeat, ZERO recalls, writer finishes with
# zero errors, checksums clean; reports the max client-visible stall.
#
#   KUBECONFIG=... CLIENT_NODE=<node> tests/k8s/pnfs-drills/mds-roll.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

WRITER=pnfs-d4-writer
trap 'cleanup_writer $WRITER' EXIT

need_env
step "preflight"
fleet_healthy

step "writer + load"
make_writer "$WRITER"
start_load "$WRITER"
sleep 8

step "kubectl rollout restart deploy/flint-pnfs-mds"
T0=$(date +%s)
kubectl rollout restart -n "$NS" deploy/flint-pnfs-mds >/dev/null
kubectl rollout status -n "$NS" deploy/flint-pnfs-mds --timeout=300s >/dev/null \
  || fail "MDS rollout did not complete"
T1=$(date +%s)
MDS_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds -o jsonpath='{.items[0].metadata.name}')
ok "MDS rolled in $(( T1 - T0 ))s (pod ${MDS_POD})"

step "post-roll MDS state"
sleep 20   # give DS heartbeats one interval to NACK + re-register
# NB: grep -q exits at first match and SIGPIPEs the producer, which
# pipefail then reports as failure — always let grep consume the
# stream and discard output instead.
BOOT=$(kubectl logs -n "$NS" "$MDS_POD" --since=10m)
echo "$BOOT" | grep "reloaded.*persisted placements" >/dev/null || fail "no placement reload in MDS log"
N_REG=$(echo "$BOOT" | grep -c "DS registered successfully" || true)
[ "${N_REG:-0}" -ge 1 ] || fail "no DS re-registrations after roll"
echo "$BOOT" | grep -E "stale data servers|Recalling|fan.out" >/dev/null && fail "recalls fired for healthy DSes"
ok "placements reloaded, ${N_REG} DS re-registrations, zero recalls"

step "writer verdict"
wait_load "$WRITER" 420
[ "$LOAD_STATUS" = "OK" ] || fail "writer status: $LOAD_STATUS"
STALL=$(max_stall "$WRITER")
verify_load "$WRITER"
ok "writer OK; max client-visible stall ${STALL}s"

printf '\n✅ PASS: MDS roll mid-workload (stall %ss, zero errors, zero recalls)\n' "$STALL"
