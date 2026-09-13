#!/usr/bin/env bash
# RETIRED PATH (2026-09-03): the lean webhook and syncer injector are
# gone — a workspace reaches a pod as ONE csi: volume served by the
# s3.csi.chert.us node driver (docs/plans/csi-node-mount-design.md §3.5).
# This rig labels pods and/or execs into an injected `flint-sync`
# container, so it no longer runs as written. The CSI delivery of lean
# is drilled by s3csi/e2e/run-s3csi.sh (S11, S13) and, across clusters,
# s3csi/e2e/multi/run-multi.sh (M3). The PROTOCOL suites here (B1-B25,
# C1-C12) remain the lean ORACLE and are to be re-targeted at the
# worker pod in flint-workers (design §10.2 S12) — not deleted, and
# never left silently green.
# The boundary-verbs kind drill (plan §5 Phases 4/5/6): the operator's
# spec verdict, the observed-state echo, the layered doors and /metrics —
# on a real cluster, against a real MinIO.
#
# House rules inherited from run-chaos.sh: every leg observes its own
# PRECONDITION or FAILS, every refusal has an accepted control, and no
# leg is allowed to pass by not looking. The first chaos run scored
# 4/10 on exactly that rule.
#
# Prereqs: kind cluster `flint-lean-boundary` with flint-sync:e2e and
# flint-lean-operator:e2e loaded (see run-chart.sh for the recipe).
set -u
cd "$(dirname "$0")"
CTX=kind-flint-lean-boundary
K="kubectl --context $CTX"
H="helm --kube-context $CTX"
PASS=0
TOTAL=7
fail() { echo "FAIL: $1"; exit 1; }
ok() { PASS=$((PASS + 1)); echo "  ok: $1"; }
note() { echo "  NOTE: $1"; }

# Condition helper: `cond <ws> <type> <field>`.
cond() {
  $K get flintleanworkspace "$1" \
    -o jsonpath="{.status.conditions[?(@.type=='$2')].$3}" 2>/dev/null
}
# Wait until a condition field matches, or time out (never sleep-and-hope).
wait_cond() { # ws type field want tries
  for _ in $(seq 1 "${5:-30}"); do
    [ "$(cond "$1" "$2" "$3")" = "$4" ] && return 0
    sleep 2
  done
  return 1
}
mc() { $K -n flint-system exec mc-assert -- mc "$@"; }

# ── bring-up ─────────────────────────────────────────────────────────
$K apply -f minio.yaml > /dev/null || fail "apply minio"
$K -n flint-system rollout status deploy/minio --timeout=180s > /dev/null || fail "minio up"
$K -n flint-system wait --for=condition=complete job/make-bucket --timeout=180s > /dev/null || fail "bucket"

$H upgrade --install flint-lean ../../flint-lean-chart -n flint-system \
  --set image.ref=flint-lean-operator:e2e --set image.pullPolicy=Never \
  --set sidecarImage.ref=flint-sync:e2e \
  --set operatorCredentialsSecret=minio-creds \
  --wait --timeout 180s > /dev/null || { $K -n flint-system logs deploy/flint-lean --tail=40; fail "helm install"; }
for i in $(seq 1 30); do
  $K get mutatingwebhookconfiguration flint-lean-inject > /dev/null 2>&1 && break
  sleep 2
done
$K get mutatingwebhookconfiguration flint-lean-inject > /dev/null 2>&1 || fail "webhook not registered"

$K -n flint-system run mc-assert --image=minio/mc --restart=Never --command -- sleep 3600 > /dev/null 2>&1
$K -n flint-system wait --for=condition=Ready pod/mc-assert --timeout=120s > /dev/null || fail "mc pod"
mc alias set m http://minio.flint-system.svc:9000 drill drillsecret > /dev/null || fail "mc alias"
echo "  bring-up: chart installed"

# Fresh objects, not re-applied ones: a CR whose spec does not change
# raises no watch event, so a status left over from an interrupted run
# would be read as this run's answer. (The claim cells in the bucket
# survive and are re-adopted — that is the designed lifecycle.)
$K delete -f boundary-workspaces.yaml --ignore-not-found --wait=true > /dev/null 2>&1
$K apply -f boundary-workspaces.yaml > /dev/null || fail "apply boundary fixtures"

# ── Phase 4: the spec verdict ─────────────────────────────────────────
wait_cond good SpecAccepted status True 60 \
  || fail "the workspace was refused: $(cond good SpecAccepted reason) — $(cond good SpecAccepted message)"
ok "B26 a coherent workspace is accepted"

# ── Phase 4: the observed-state echo ─────────────────────────────────
$K wait --for=condition=Ready pod/agent-good --timeout=300s > /dev/null \
  || { $K describe pod agent-good | tail -20; fail "agent-good never Ready"; }
# No poke here on purpose: this leg's claim is that the operator picks
# the echo up ON ITS OWN CADENCE. Two observation intervals of slack.
wait_cond good SyncerObserved status True 130 \
  || fail "the running syncer never echoed (reason $(cond good SyncerObserved reason))"
SEQ=$($K get flintleanworkspace good -o jsonpath='{.status.citedSeq}')
[ -n "$SEQ" ] || fail "status.citedSeq is empty — the echo did not reach status"
ok "B32 the lease-heartbeat echo reaches status (citedSeq=$SEQ)"

# ── Phase 5: the gateway door (the inbox document's two fields) ──────
INBOX=tenants/good/.flint/lean/inbox
$K exec agent-good -c agent -- test -f /workspace/.flint/publish.ack 2>/dev/null \
  && fail "publish.ack existed BEFORE the request — the leg cannot prove anything"
NOW=$(date +%s)
echo "{\"entries\":[],\"window\":null,\"boundary_request\":{\"requested_unix\":$NOW,\"requestor\":\"ci@drill\"}}" \
  | $K -n flint-system exec -i mc-assert -- mc pipe "m/agentws/$INBOX" > /dev/null || fail "write inbox doc"
ACK=""
for i in $(seq 1 30); do
  ACK=$($K exec agent-good -c agent -- cat /workspace/.flint/publish.ack 2>/dev/null)
  case "$ACK" in *ci@drill*) break ;; esac
  sleep 2
done
case "$ACK" in
  *ci@drill*) ;;
  *) fail "the gateway boundary request produced no ack naming the requestor: $ACK" ;;
esac
ok "B34 a gateway boundary request is honored as a publish sentinel"

# D14: carried, NEVER executed. The failing control is the tree hash.
HASH_BEFORE=$($K exec agent-good -c agent -- sh -c 'ls -la /workspace | md5sum')
NOW=$(date +%s)
echo "{\"entries\":[],\"window\":null,\"sync_request\":{\"requested_unix\":$NOW,\"requestor\":\"ci@drill\"}}" \
  | $K -n flint-system exec -i mc-assert -- mc pipe "m/agentws/$INBOX" > /dev/null || fail "write inbox doc"
CARRIED=""
for i in $(seq 1 30); do
  CARRIED=$($K exec agent-good -c agent -- cat /workspace/.flint/remote.seq 2>/dev/null)
  case "$CARRIED" in *ci@drill*) break ;; esac
  sleep 2
done
case "$CARRIED" in
  *ci@drill*) ;;
  *) fail "the sync request was not carried into the ticker: $CARRIED" ;;
esac
HASH_AFTER=$($K exec agent-good -c agent -- sh -c 'ls -la /workspace | md5sum')
[ "$HASH_BEFORE" = "$HASH_AFTER" ] \
  || fail "the syncer MUTATED the tree on a remote's say-so (D14 violated)"
ok "B35 a gateway sync request is carried, and the tree is byte-identical"

# ── Phase 5: the UDS door, and that it shares ONE consume path ───────
$K exec agent-good -c flint-sync -- test -S /workspace/.flint-sync/ctl.sock \
  || fail "the control socket was never bound"
OUT=$($K exec agent-good -c flint-sync -- /usr/local/bin/flint-sync ctl boundary 2>&1)
case "$OUT" in
  *'"status":"ok"'*) ;;
  *) fail "the UDS boundary did not answer ok: $OUT" ;;
esac
case "$OUT" in
  *'uds:'*) ;;
  *) fail "the socket's ack does not name a uds nonce — it did not go through the sentinel path: $OUT" ;;
esac
ok "B36 the UDS door answers synchronously through the sentinel consume path"

# ── Phase 6: /metrics, and the label rule ────────────────────────────
METRICS=$($K exec agent-good -c agent -- wget -q -O - http://127.0.0.1:9847/metrics 2>/dev/null)
SERIES=$(printf '%s\n' "$METRICS" | grep -c '^flint_lean_')
[ "$SERIES" -ge 9 ] || fail "/metrics returned $SERIES series (expected >= 9): $METRICS"
BADLABEL=$(printf '%s\n' "$METRICS" | grep '^flint_lean_' \
  | sed 's/^[^{]*{//; s/}.*//' | tr ',' '\n' | cut -d= -f1 | sort -u \
  | grep -vE '^(workspace|namespace)$' | head -1)
[ -z "$BADLABEL" ] || fail "a series carries the label key '$BADLABEL' beyond {workspace,namespace}"
printf '%s\n' "$METRICS" | grep -q 'flint_lean_fenced{workspace="good",namespace="default"} 0' \
  || fail "the exposition does not carry the fenced gauge with the expected labels"
ok "B37 /metrics serves $SERIES series, label keys exactly {workspace,namespace}"

# ── Phase 6: the bind collision degrades, it does not crash ──────────
$K wait --for=condition=Ready pod/agent-portclash --timeout=300s > /dev/null \
  || { $K describe pod agent-portclash | tail -20; fail "the port collision took the workspace down"; }
wait_cond portclash MetricsExposed reason PortUnavailable 130 \
  || fail "a bind collision was not reported (reason '$(cond portclash MetricsExposed reason)')"
BODY=""
for i in $(seq 1 30); do
  BODY=$(mc cat m/agentws/tenants/portclash/files/still-works.txt 2>/dev/null)
  [ "$BODY" = "alive" ] && break
  sleep 3
done
[ "$BODY" = "alive" ] || fail "the workspace stopped publishing after losing the metrics port"
ok "B38 a lost metrics port degrades to a condition; the workspace keeps publishing"

echo
echo "flint-lean boundary drill: $PASS/$TOTAL legs green"
[ "$PASS" -eq "$TOTAL" ] || exit 1
