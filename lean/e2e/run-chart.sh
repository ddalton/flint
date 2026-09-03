#!/usr/bin/env bash
# RETIRED PATH (2026-09-03): the lean webhook and sidecar injector are
# gone — a workspace reaches a pod as ONE csi: volume served by the
# s3.csi.chert.us node driver (docs/plans/csi-node-mount-design.md §3.5).
# This rig labels pods and/or execs into an injected `flint-sync`
# container, so it no longer runs as written. The CSI delivery of lean
# is drilled by s3csi/e2e/run-s3csi.sh (S11, S13) and, across clusters,
# s3csi/e2e/multi/run-multi.sh (M3). The PROTOCOL suites here (B1-B25,
# C1-C12) remain the lean ORACLE and are to be re-targeted at the
# worker pod in flint-workers (design §10.2 S12) — not deleted, and
# never left silently green.
# The CHART e2e: install flint-lean via helm (not raw manifests) and
# re-run the same acceptance. A chart that renders is not a chart that
# works — this leg is what proves the templates wire the operator, its
# RBAC, the webhook Service and the sidecar image reference correctly.
#
# Prereqs: kind cluster `flint-lean-chart` with flint-sync:e2e and
# flint-lean-operator:e2e loaded.
set -u
cd "$(dirname "$0")"
CTX=kind-flint-lean-chart
K="kubectl --context $CTX"
H="helm --kube-context $CTX"
PASS=0
fail() { echo "FAIL: $1"; exit 1; }
ok() { PASS=$((PASS + 1)); echo "  ok: $1"; }

# MinIO + bucket only (the operator/webhook/RBAC come from the chart).
$K apply -f minio.yaml > /dev/null || fail "apply minio"
$K -n flint-system rollout status deploy/minio --timeout=180s > /dev/null || fail "minio up"
$K -n flint-system wait --for=condition=complete job/make-bucket --timeout=180s > /dev/null || fail "bucket"
ok "MinIO + bucket up"

$H install flint-lean ../../flint-lean-chart -n flint-system \
  --set image.ref=flint-lean-operator:e2e \
  --set image.pullPolicy=Never \
  --set sidecarImage.ref=flint-sync:e2e \
  --set operatorCredentialsSecret=minio-creds \
  --wait --timeout 180s > /dev/null || {
    $K -n flint-system logs deploy/flint-lean --tail=30
    fail "helm install"
  }
ok "chart installed (operator Ready behind its own readiness probe)"

# The CRD came from crds/ at install time; the webhook registration is
# the operator's own doing.
$K get crd flintleanworkspaces.chert.us > /dev/null 2>&1 || fail "CRD not installed by the chart"
for i in $(seq 1 30); do
  $K get mutatingwebhookconfiguration flint-lean-inject > /dev/null 2>&1 && break
  sleep 2
done
$K get mutatingwebhookconfiguration flint-lean-inject > /dev/null 2>&1 || fail "webhook not registered"
# The registration must point at the CHART's Service, not a hardcoded name.
SVC=$($K get mutatingwebhookconfiguration flint-lean-inject -o jsonpath='{.webhooks[0].clientConfig.service.name}')
[ "$SVC" = "flint-lean" ] || fail "webhook points at Service '$SVC', not the chart's"
ok "CRD + webhook registered against the chart's Service"

$K apply -f workspace.yaml > /dev/null || fail "apply workspace + pod"
for i in $(seq 1 60); do
  PHASE=$($K get flintleanworkspace proj1 -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$PHASE" = "Claimed" ] || [ "$PHASE" = "Adopted" ] && break
  sleep 2
done
[ "$PHASE" = "Claimed" ] || [ "$PHASE" = "Adopted" ] || fail "workspace phase '$PHASE'"
ok "workspace $PHASE (operator principal reached the bucket)"

# The chart's sidecarImage value must be what actually gets injected.
IMG=$($K get pod agent-1 -o jsonpath='{.spec.initContainers[0].image}')
[ "$IMG" = "flint-sync:e2e" ] || fail "injected image is '$IMG', not the chart's sidecarImage"
ok "chart's sidecarImage is the injected image"

$K wait --for=condition=Ready pod/agent-1 --timeout=300s > /dev/null || {
  $K describe pod agent-1 | tail -20; fail "agent never Ready"
}
ok "agent started behind the checkout gate"

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
ok "publish reached the bucket through the chart-installed stack"

if $K run bad-agent --image=busybox:stable --labels=chert.us/lean-workspace=no-such-ws \
     --restart=Never --command -- sleep 1 > /dev/null 2>&1; then
  fail "a pod naming a missing workspace was ADMITTED — the gate is vacuous"
fi
ok "missing-workspace pod refused at admission"

echo
echo "flint-lean chart e2e: $PASS/8 legs green"
