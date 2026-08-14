#!/usr/bin/env bash
# kind WITNESS pass — the composition witness against a REAL API server,
# AS THE SERVICE ACCOUNT THE CHART CREATES.
#
# WHY THIS EXISTS, stated first:
#
#   The witness decides which of a replicated volume's two targets
#   composes it, and it decides by compare-and-swap on one ConfigMap.
#   Every unit test in witness_kube.rs runs against an in-memory store,
#   so every one of them ASSUMES the answer to the only question that
#   matters: does the API server actually refuse a merge-patch whose
#   resourceVersion has moved? If it does not, the CAS is decoration and
#   two targets can both believe they compose the volume.
#
#   The second thing only a live server knows is whether the Role in the
#   chart grants the verbs the store calls. A forgotten verb is a 403
#   that appears for the first time during a failover. So this pass runs
#   the real code path under the SERVICE ACCOUNT'S OWN TOKEN — not as
#   cluster-admin — and a missing verb fails here instead.
#
# WHAT IT DOES NOT COVER: the data path. The Docker VM kernel is far
# below the pnfs-block client floor (6.11); see kind-chart-pass.sh, which
# validates the chart SURFACE against the same kind of cluster. This pass
# needs no flint image and runs no flint pod — it is the driver's own
# witness code, on this machine, talking to kind's API server.
#
# Usage:  tests/regression/kind-witness-pass.sh
#         KEEP=1 tests/regression/kind-witness-pass.sh   # leave the cluster up
set -uo pipefail

CLUSTER="${CLUSTER:-flint-witness-pass}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$ROOT/flint-csi-driver-chart"
DRIVER="$ROOT/spdk-csi-driver"
NS=flint-system
SA=flint-pnfs-mds
ADMIN_KUBECONFIG="$(mktemp -t flint-kind-admin.XXXXXX)"
SA_KUBECONFIG="$(mktemp -t flint-kind-sa.XXXXXX)"
export KUBECONFIG="$ADMIN_KUBECONFIG"

pass() { echo "✓ $*"; }
fail() { echo "✗ $*"; exit 1; }
say()  { echo; echo "── $* ──────────────────────────────────────────"; }

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
    rm -f "$ADMIN_KUBECONFIG" "$SA_KUBECONFIG"
  else
    echo "· cluster kept:  kind get kubeconfig --name $CLUSTER"
    echo "· SA kubeconfig: $SA_KUBECONFIG"
  fi
}
trap cleanup EXIT

# ── 0. preflight, and BUILD FIRST ────────────────────────────────────
for t in kind kubectl helm docker cargo; do
  command -v "$t" >/dev/null || fail "$t not installed"
done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"

# Compile before spending a cluster on a build error. This also means the
# `cargo test` at the end is a run, not a build, so its output is legible.
say "building the live drill"
cargo test --manifest-path "$DRIVER/Cargo.toml" --test kube_witness_live --no-run >/dev/null 2>&1 \
  || fail "the live drill does not compile (run it by hand for the errors)"
pass "kube_witness_live built"

# ── 1. a one-node cluster; no images, no workloads ───────────────────
say "cluster"
if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
  kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 \
    || fail "kind create cluster failed (docker memory? $(docker info --format '{{.MemTotal}}' 2>/dev/null) bytes)"
fi
kind get kubeconfig --name "$CLUSTER" > "$ADMIN_KUBECONFIG" 2>/dev/null \
  || fail "kind get kubeconfig"
kubectl create namespace "$NS" >/dev/null 2>&1 || true
SRV=$(kubectl version -o json 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["serverVersion"]["gitVersion"])' 2>/dev/null)
pass "kind cluster up ($SRV)"

# ── 2. THE CHART'S OWN RBAC, applied for real ────────────────────────
#
# Rendered from the chart with the witness enabled — not hand-written
# here. A drill that grants its own permissions proves nothing about
# what an operator would actually install.
say "the chart's witness RBAC"
RBAC=$(helm template flint "$CHART" --namespace "$NS" \
        --set snapshotClass.enabled=false \
        --set pnfs.enabled=true --set pnfs.server.enabled=true \
        --set pnfs.blockLayout.enabled=true \
        --set pnfs.server.mds.blockExport.enabled=true \
        --set pnfs.server.mds.blockExport.lvstore=lvs_flint \
        --set pnfs.server.mds.blockExport.traddr=10.0.0.9 \
        --set pnfs.server.mds.blockExport.witness.enabled=true \
        --show-only templates/rbac.yaml 2>&1)
[ $? -eq 0 ] || { printf '%s\n' "$RBAC" | tail -5; fail "helm template (rbac) failed"; }
case "$RBAC" in
  *"flint-pnfs-mds-witness"*) ;;
  *) fail "the rendered RBAC has no witness Role — is the values path still blockExport.witness.enabled?" ;;
esac
printf '%s\n' "$RBAC" | kubectl apply -f - >/dev/null 2>&1 \
  || fail "the API server refused the chart's RBAC"
pass "ServiceAccount, Role and RoleBinding applied from the chart"

# ── 3. the credential, and what it may do ────────────────────────────
#
# `auth can-i` is the server's own answer, asked before we depend on it —
# so a missing verb is named here rather than surfacing as a mid-drill
# failure whose cause has to be inferred.
say "what the ServiceAccount may do"
for v in get list create update patch delete; do
  ans=$(kubectl auth can-i "$v" configmaps -n "$NS" \
          --as="system:serviceaccount:$NS:$SA" 2>/dev/null)
  [ "$ans" = "yes" ] || fail "the witness Role does not grant '$v' on configmaps in $NS — \
the store calls it, so this is a 403 waiting for a failover"
done
pass "get/list/create/update/patch/delete on configmaps in $NS"

ans=$(kubectl auth can-i list configmaps -n default \
        --as="system:serviceaccount:$NS:$SA" 2>/dev/null)
[ "$ans" = "no" ] || fail "the witness credential reaches OUTSIDE its namespace ($ans) — \
the Role is supposed to be namespaced"
pass "and nothing in another namespace"

# ── 4. a kubeconfig that IS the ServiceAccount ───────────────────────
#
# The client certificate must be UNSET, not merely joined by a token:
# with both present the certificate wins and the whole drill would
# quietly run as cluster-admin. (Leg 2 of the drill would catch it — it
# asserts a cross-namespace read is refused — but a check that depends on
# a later assertion to notice its own failure is not a check.)
say "minting the ServiceAccount token"
TOKEN=$(kubectl create token "$SA" -n "$NS" --duration=1h 2>/dev/null)
[ -n "$TOKEN" ] || fail "kubectl create token $SA failed (API server below 1.24?)"
cp "$ADMIN_KUBECONFIG" "$SA_KUBECONFIG"
KUSER=$(kubectl --kubeconfig="$SA_KUBECONFIG" config view -o jsonpath='{.contexts[0].context.user}')
kubectl --kubeconfig="$SA_KUBECONFIG" config unset "users.$KUSER.client-certificate-data" >/dev/null
kubectl --kubeconfig="$SA_KUBECONFIG" config unset "users.$KUSER.client-key-data" >/dev/null
kubectl --kubeconfig="$SA_KUBECONFIG" config set-credentials "$KUSER" --token="$TOKEN" >/dev/null
kubectl --kubeconfig="$SA_KUBECONFIG" config set-context --current --namespace="$NS" >/dev/null

WHO=$(kubectl --kubeconfig="$SA_KUBECONFIG" auth whoami -o jsonpath='{.status.userInfo.username}' 2>/dev/null)
case "$WHO" in
  "system:serviceaccount:$NS:$SA") pass "the drill will run as $WHO" ;;
  "") # older servers have no `auth whoami`; fall back to the refusal itself
      kubectl --kubeconfig="$SA_KUBECONFIG" get cm -n default >/dev/null 2>&1 \
        && fail "the SA kubeconfig can read another namespace — it is still admin"
      pass "running as the ServiceAccount (verified by refusal; no auth whoami)" ;;
  *) fail "the SA kubeconfig authenticates as '$WHO', not the ServiceAccount" ;;
esac

# ── 5. THE DRILL ─────────────────────────────────────────────────────
say "the witness, against $SRV, as $SA"
KUBECONFIG="$SA_KUBECONFIG" FLINT_WITNESS_NS="$NS" \
  cargo test --manifest-path "$DRIVER/Cargo.toml" --test kube_witness_live \
  -- --ignored --test-threads=1 --nocapture
DRILL=$?
[ "$DRILL" -eq 0 ] || fail "the witness drill FAILED against a real API server"

# ── 6. it cleans up after itself ─────────────────────────────────────
#
# One ConfigMap per volume: a sweep that does not sweep is a leak that
# outlives every volume the cluster ever had.
LEFT=$(kubectl get cm -n "$NS" -l flint.io/kind -o name 2>/dev/null | wc -l | tr -d ' ')
[ "$LEFT" = "0" ] || {
  kubectl get cm -n "$NS" -l flint.io/kind -o name 2>/dev/null | sed 's/^/    /'
  fail "$LEFT witness object(s) survived the drill"
}
pass "no witness objects left behind"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " ✅ kind witness pass — the compare-and-swap is the API server's,"
echo " the chart's Role grants exactly what the store calls, and the"
echo " record is swept. Data path NOT covered here (kernel floor)."
echo "══════════════════════════════════════════════════════════════════"
