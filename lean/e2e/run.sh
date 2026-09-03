#!/usr/bin/env bash
# RETIRED PATH (2026-09-03): the lean webhook and sidecar injector are
# gone — a workspace reaches a pod as ONE csi: volume served by the
# s3.flint.io node driver (docs/plans/csi-node-mount-design.md §3.5).
# This rig and its siblings (run-agent.sh, run-boundary.sh,
# run-verbs.sh, run-chaos.sh) still label pods and exec into an
# injected `flint-sync` container, so they no longer run as written.
# The CSI delivery of lean is drilled by s3csi/e2e/run-s3csi.sh legs
# S11/S13; the protocol suites here (B1-B25, C1-C12) are the lean
# ORACLE and are to be re-targeted at the worker pod in flint-workers
# (design §10.2 S12) — not deleted, not silently green.
# The lean operator kind e2e (plan §5 Phase 4): injection, the
# checkout gate, a real publish through MinIO, and the failing control
# (a pod naming a missing workspace MUST be refused — failurePolicy
# Fail with teeth, not vacuously green).
#
# Prereqs: a kind cluster (kind create cluster --name flint-lean-e2e),
# images flint-sync:e2e + flint-lean-operator:e2e kind-loaded.
set -u
cd "$(dirname "$0")"
CTX=kind-flint-lean-e2e
K="kubectl --context $CTX"
PASS=0
fail() { echo "FAIL: $1"; exit 1; }
ok() { PASS=$((PASS + 1)); echo "  ok: $1"; }

$K apply -f minio.yaml -f operator.yaml > /dev/null || fail "apply operator stack"
$K -n flint-system rollout status deploy/minio --timeout=180s > /dev/null || fail "minio up"
$K -n flint-system wait --for=condition=complete job/make-bucket --timeout=180s > /dev/null || fail "bucket made"
$K -n flint-system rollout status deploy/flint-lean-operator --timeout=180s > /dev/null || fail "operator up"
# The webhook needs its endpoints; wait until the registration exists.
for i in $(seq 1 30); do
  $K get mutatingwebhookconfiguration flint-lean-inject > /dev/null 2>&1 && break
  sleep 2
done
$K get mutatingwebhookconfiguration flint-lean-inject > /dev/null || fail "webhook registered"
ok "operator + MinIO + webhook up"

$K apply -f workspace.yaml > /dev/null || fail "apply workspace + agent pod"

# 1. The CR reaches Claimed (claim cell created, posture verified).
for i in $(seq 1 60); do
  PHASE=$($K get flintleanworkspace proj1 -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$PHASE" = "Claimed" ] || [ "$PHASE" = "Adopted" ] && break
  sleep 2
done
[ "$PHASE" = "Claimed" ] || [ "$PHASE" = "Adopted" ] || fail "workspace phase is '$PHASE'"
ok "workspace $PHASE"

# 2. The webhook injected the NATIVE sidecar.
INJ=$($K get pod agent-1 -o jsonpath='{.spec.initContainers[0].name}/{.spec.initContainers[0].restartPolicy}')
[ "$INJ" = "flint-sync/Always" ] || fail "sidecar not injected as a native sidecar (got '$INJ')"
ok "native sidecar injected"

# 3. The gate: the pod goes Ready, and the agent's own first command
#    asserts the marker existed before it ran (the pod FAILS otherwise).
$K wait --for=condition=Ready pod/agent-1 --timeout=300s > /dev/null || {
  $K describe pod agent-1 | tail -20
  fail "agent pod never became Ready"
}
ok "agent started behind the checkout gate"

# 4. The publish: the agent's file reaches the bucket within the floor.
$K -n flint-system run mc-assert --image=minio/mc --restart=Never --command -- sleep 600 > /dev/null 2>&1
$K -n flint-system wait --for=condition=Ready pod/mc-assert --timeout=120s > /dev/null || fail "mc pod"
$K -n flint-system exec mc-assert -- mc alias set m http://minio.flint-system.svc:9000 drill drillsecret > /dev/null
BODY=""
for i in $(seq 1 30); do
  BODY=$($K -n flint-system exec mc-assert -- mc cat m/agentws/tenants/proj1/files/agent.txt 2>/dev/null)
  [ "$BODY" = "hello-from-agent" ] && break
  sleep 3
done
[ "$BODY" = "hello-from-agent" ] || fail "agent.txt never published (got '$BODY')"
ok "agent's write published through the barrier"

# 5. The manifest cites it (the coherent view, not just the object).
MAN=$($K -n flint-system exec mc-assert -- mc cat m/agentws/tenants/proj1/.flint/lean/manifest 2>/dev/null)
echo "$MAN" | grep -c '"agent.txt"' > /dev/null || fail "manifest does not cite agent.txt"
ok "manifest cites the publish"

# 6. THE FAILING CONTROL: a pod opting into a MISSING workspace must be
#    REFUSED at admission (this leg fails if the webhook is vacuous).
if $K run bad-agent --image=busybox:stable --labels=flint.io/lean-workspace=no-such-ws \
     --restart=Never --command -- sleep 1 > /dev/null 2>&1; then
  fail "a pod naming a missing workspace was ADMITTED — the gate is vacuous"
fi
ok "missing-workspace pod refused at admission (failurePolicy has teeth)"

echo
echo "lean operator e2e: $PASS/7 legs green"
