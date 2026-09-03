#!/usr/bin/env bash
# RETIRED PATH (2026-09-03): the lean webhook and sidecar injector are
# gone — a workspace reaches a pod as ONE csi: volume served by the
# s3.chert.us node driver (docs/plans/csi-node-mount-design.md §3.5).
# This rig labels pods and/or execs into an injected `flint-sync`
# container, so it no longer runs as written. The CSI delivery of lean
# is drilled by s3csi/e2e/run-s3csi.sh (S11, S13) and, across clusters,
# s3csi/e2e/multi/run-multi.sh (M3). The PROTOCOL suites here (B1-B25,
# C1-C12) remain the lean ORACLE and are to be re-targeted at the
# worker pod in flint-workers (design §10.2 S12) — not deleted, and
# never left silently green.
# The boundary-verbs kind drill (plan §5 Phases 4/5/6): the operator's
# refusals, the observed-state echo, the layered doors and /metrics —
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
TOTAL=14
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
# Gated mode REQUIRES versioning; without it every gated leg below is
# testing the refusal path by accident.
mc version enable m/agentws > /dev/null 2>&1 || fail "enable bucket versioning"
[ "$(mc version info m/agentws | grep -ci enabled)" -ge 1 ] || fail "versioning not Enabled"
echo "  bring-up: chart installed, bucket versioned"

# Rig reset: this drill PLANTS a hostile lifecycle rule, and a leftover
# from an interrupted run would poison its own accepted control. Remove
# any rule flint did not write before starting.
for id in $(mc ilm rule ls m/agentws --json 2>/dev/null | grep -o '"ID":"[^"]*"' | cut -d'"' -f4 | grep -v '^flint-lean'); do
  mc ilm rule rm --id "$id" m/agentws > /dev/null 2>&1
done

# Fresh objects, not re-applied ones: a CR whose spec does not change
# raises no watch event, so a status left over from an interrupted run
# would be read as this run's answer. (The claim cells in the bucket
# survive and are re-adopted — that is the designed lifecycle.)
$K delete -f boundary-workspaces.yaml --ignore-not-found --wait=true > /dev/null 2>&1
$K apply -f boundary-workspaces.yaml > /dev/null || fail "apply boundary fixtures"

# ── Phase 4: the refusals, each with its accepted control ────────────
# The control comes FIRST: a refusal suite whose accepted case does not
# pass is a suite that says no to everything.
wait_cond good BoundaryModeAccepted status True 60 \
  || fail "the ACCEPTED control was refused: $(cond good BoundaryModeAccepted reason) — $(cond good BoundaryModeAccepted message)"
ok "accepted control: a coherent gated workspace is accepted"

wait_cond nolag BoundaryModeAccepted reason LagBoundRequired 30 \
  || fail "gated-without-a-lag-bound was accepted (reason '$(cond nolag BoundaryModeAccepted reason)')"
[ "$(cond nolag BoundaryModeAccepted status)" = "False" ] || fail "nolag reason set but status not False"
ok "B26 gated without visibilityLagBoundSecs is refused"

wait_cond shortret BoundaryModeAccepted reason RetentionTooShort 30 \
  || fail "a retention shorter than one staging window was accepted"
case "$(cond shortret BoundaryModeAccepted message)" in
  *7210s*) ;;
  *) fail "the refusal does not name the window it violated" ;;
esac
ok "B27 retention cross-validation refuses, naming the window"

wait_cond bigbacklog BoundaryModeAccepted reason GraceTooShort 30 \
  || fail "an undrainable backlog cap was accepted"
case "$(cond bigbacklog BoundaryModeAccepted message)" in
  *stagedBacklogCapBytes*) ;;
  *) fail "the refusal does not name the knob to lower" ;;
esac
ok "B28 a backlog no spot reclaim can drain is refused, naming the knob"

# ── Phase 4: the backstop is really installed, in the bucket ─────────
wait_cond good VersionRetentionProvisioned status True 30 \
  || fail "the noncurrent backstop was never provisioned"
RULES=$(mc ilm rule ls m/agentws --json 2>/dev/null)
case "$RULES" in
  *tenants/good/files/*) ;;
  *) fail "no lifecycle rule covering tenants/good/files/ exists in the BUCKET: $RULES" ;;
esac
ok "B29 the 30-day noncurrent backstop exists in the live bucket config"

# ── Phase 4: a customer's shorter rule refuses gated, both ways ──────
# On the `lifecycle` workspace, which carries no pod: these legs
# RECREATE the CR, and recreating `good` would take its agent — and
# every later leg — with it.
#
# Recreation is how the drill forces a POSTURE pass. The bucket-side
# checks ride the slow cadence by design (§2.6): re-asking every two
# minutes whether a bucket's lifecycle rules changed would multiply
# fleet operator traffic to re-answer a question that moves on the
# timescale of an admin edit. The exposure that implies — up to one
# posture cadence between someone arming a 1-day rule and the refusal —
# is immaterial against a rule that reaps at DAY granularity.
wait_cond lifecycle BoundaryModeAccepted status True 60 \
  || fail "the lifecycle fixture was refused before the rule was planted"
mc ilm rule add m/agentws --prefix "tenants/lifecycle/" \
   --noncurrent-expire-days 1 > /dev/null 2>&1 \
   || fail "could not plant the hostile lifecycle rule"
$K delete flintleanworkspace lifecycle --wait=true > /dev/null 2>&1
$K apply -f boundary-workspaces.yaml > /dev/null
wait_cond lifecycle BoundaryModeAccepted reason ShorterNoncurrentRule 40 \
  || fail "a 1-day noncurrent rule over the prefix did NOT refuse gated mode"
ok "B30 a shorter covering rule refuses gated mode (the destroyer flint never wrote)"

BAD=$(mc ilm rule ls m/agentws --json | grep -o '"ID":"[^"]*"' | cut -d'"' -f4 | grep -v '^flint-lean' | head -1)
[ -n "$BAD" ] && mc ilm rule rm --id "$BAD" m/agentws > /dev/null 2>&1
$K delete flintleanworkspace lifecycle --wait=true > /dev/null 2>&1
$K apply -f boundary-workspaces.yaml > /dev/null
wait_cond lifecycle BoundaryModeAccepted status True 60 \
  || fail "removing the hostile rule did not restore acceptance — the refusal did not track the rule"
ok "B31 removing it restores acceptance (the refusal tracks the rule, not a typo)"

# ── Phase 4: the observed-state echo ─────────────────────────────────
$K wait --for=condition=Ready pod/agent-good --timeout=300s > /dev/null \
  || { $K describe pod agent-good | tail -20; fail "agent-good never Ready"; }
# No poke here on purpose: this leg's claim is that the operator picks
# the echo up ON ITS OWN CADENCE. Two observation intervals of slack.
wait_cond good BoundaryModeActive status True 130 \
  || fail "the running sidecar never echoed its mode (reason $(cond good BoundaryModeActive reason))"
SEQ=$($K get flintleanworkspace good -o jsonpath='{.status.citedSeq}')
[ -n "$SEQ" ] || fail "status.citedSeq is empty — the echo did not reach status"
ok "B32 the lease-heartbeat echo reaches status (citedSeq=$SEQ)"

# Spec vs RUNNING binary. Patching the CR does NOT re-stamp a running
# pod's env (§2.6's propagation semantics), so the echo must disagree.
$K patch flintleanworkspace good --type=merge -p '{"spec":{"boundaryMode":"hybrid"}}' > /dev/null
wait_cond good BoundaryModeActive reason ModeMismatch 130 \
  || fail "spec/observed mismatch did NOT flip BoundaryModeActive"
[ "$(cond good BoundaryModeActive status)" = "False" ] || fail "ModeMismatch without status False"
ok "B33 a spec the running binary is not honoring flips BoundaryModeActive"
$K patch flintleanworkspace good --type=merge -p '{"spec":{"boundaryMode":"gated"}}' > /dev/null
wait_cond good BoundaryModeActive status True 130 || note "mode did not settle back to True"
# The message must name the BINARY, not just disagree — an operator has
# to know which side to move.
case "$(cond good BoundaryModeActive message)" in
  *sidecar*) ;;
  *) note "the mismatch message does not name the sidecar version" ;;
esac

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
  || fail "the sidecar MUTATED the tree on a remote's say-so (D14 violated)"
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
[ "$SERIES" -ge 13 ] || fail "/metrics returned $SERIES series (expected >= 13): $METRICS"
BADLABEL=$(printf '%s\n' "$METRICS" | grep '^flint_lean_' \
  | sed 's/^[^{]*{//; s/}.*//' | tr ',' '\n' | cut -d= -f1 | sort -u \
  | grep -vE '^(workspace|namespace)$' | head -1)
[ -z "$BADLABEL" ] || fail "a series carries the label key '$BADLABEL' beyond {workspace,namespace}"
printf '%s\n' "$METRICS" | grep -q 'flint_lean_boundary_mode{workspace="good",namespace="default"} 2' \
  || fail "the exposition does not report gated mode with the expected labels"
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
