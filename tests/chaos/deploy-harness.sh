#!/usr/bin/env bash
# Deploy / reset / tear down the Postgres chaos harness.
#
#   SC=flint MODE=RWO ./deploy-harness.sh up        # deploy + wait + warm
#   ./deploy-harness.sh down                        # delete ns, wait for PVs
#   SC=flint-r2 MODE=RWO ./deploy-harness.sh reset  # down + up
#   SC=flint MODE=RWX WITNESS=1 ./deploy-harness.sh up
#
# MODE: RWO | RWOP | RWX (maps to PVC accessModes). SCALE: pgbench scale (200).
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh
. ./manifests.sh

SC=${SC:-flint}
MODE=${MODE:-RWO}
case "$MODE" in
  RWO)  ACCESS_MODE=ReadWriteOnce ;;
  RWOP) ACCESS_MODE=ReadWriteOncePod ;;   # pod-fenced: kubelet refuses 2nd pod (drill 1.3b)
  RWX)  ACCESS_MODE=ReadWriteMany ;;
  *) fail "MODE must be RWO, RWOP or RWX" ;;
esac
export NS SC ACCESS_MODE

up() {
  need_env
  step "deploying harness (ns=$NS sc=$SC mode=$MODE scale=${SCALE:-200})"
  { emit_ns; echo ---; emit_secret; echo ---; emit_svc; echo ---; emit_sts; echo ---; emit_load; } \
    | kubectl apply -f - >/dev/null || fail "apply failed"
  ok "core objects applied"

  kubectl wait --for=condition=Ready pod/$PG -n "$NS" --timeout=300s >/dev/null \
    || fail "pg-0 never became Ready (check PVC binding on SC=$SC)"
  ok "pg-0 Ready on $(pg_node) (pv $(pg_pv))"

  emit_init_job | kubectl apply -f - >/dev/null
  kubectl wait --for=condition=complete job/pg-init -n "$NS" --timeout=600s >/dev/null \
    || fail "pg-init job did not complete"
  ok "schema + pgbench -i -s ${SCALE:-200} done"

  if [ "${WITNESS:-0}" = "1" ]; then
    emit_witness | kubectl apply -f - >/dev/null
    kubectl wait --for=condition=Available deploy/witness -n "$NS" --timeout=180s >/dev/null \
      || fail "witness never became Available"
    ok "witness up on $(kubectl get pod -n "$NS" -l app=witness -o jsonpath='{.items[0].spec.nodeName}')"
  fi

  # Ledger must be acking before any drill starts.
  local i
  for i in $(seq 1 24); do
    [ -n "$(load_pod)" ] \
      && kubectl exec -n "$NS" "$(load_pod)" -- sh -c 'test -s /acked/acked.log' 2>/dev/null \
      && break
    sleep 5
  done
  harness_healthy
  step "harness up"
}

down() {
  need_env
  step "tearing down harness"
  kubectl delete job pg-init -n "$NS" --ignore-not-found --wait=false >/dev/null 2>&1
  kubectl delete ns "$NS" --ignore-not-found --timeout=300s \
    || fail "namespace delete hung — PV finalizer? (that is itself a finding)"
  # PVs are reclaimPolicy Delete: watch them actually go.
  local i left
  for i in $(seq 1 36); do
    left=$(kubectl get pv -o json | jq -r --arg ns "$NS" \
      '[.items[] | select(.spec.claimRef.namespace==$ns)] | length')
    [ "$left" = "0" ] && break
    sleep 5
  done
  [ "$left" = "0" ] || fail "PVs still present after ns delete: $left (finalizer hang = finding)"
  ok "namespace + PVs gone"
}

case "${1:-}" in
  up) up ;;
  down) down ;;
  reset) down; up ;;
  *) echo "usage: [SC=flint] [MODE=RWO|RWX] [WITNESS=1] $0 up|down|reset"; exit 1 ;;
esac
