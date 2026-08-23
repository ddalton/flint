#!/usr/bin/env bash
# Flint-lite — "Ready is never published without an address", on a REAL cluster.
#
# THE PROPERTY. `status.phase: Ready` is what a consumer waits on before
# reading `status.address` and mounting it. Publishing Ready with an
# empty address hands them nothing. Before v1.36.0 the operator derived
# the phase from the Deployment alone, so a `type: LoadBalancer` share
# went Ready the moment its pod was available — minutes before the cloud
# provider populated `status.loadBalancer.ingress`.
#
# WHY A CLUSTER AND NOT kind. The window is the gap between "pod
# available" and "the Service has an ingress address". On kind both are
# instant-or-never, so the window is not reachable in a way that
# distinguishes the two builds. On a real cluster the pod comes up on a
# real node against a real registry pull while the Service genuinely has
# no address at all.
#
# HONEST NOTE ON THE ADDRESS TRANSITION. A trove cluster runs no
# cloud-controller-manager, so a `type: LoadBalancer` Service pends
# FOREVER. That is a feature here: legs 1-2 get an infinitely wide
# window, which is the strongest possible form of the test. For leg 3
# (Ready must still be REACHABLE) something has to supply the address.
# We prefer Cilium's own LB-IPAM — a real third-party controller filling
# in the ingress on its own schedule — and fall back to writing the
# status subresource exactly as a CCM would. The fallback is SIMULATED
# and the leg says so in its output.
#
# THE DIFFERENTIAL IS THE POINT. Every leg here would pass against a
# broken build if it were run alone, so leg 1 runs the SHIPPED PRE-FIX
# operator and REQUIRES the bug to appear. If leg 1 does not reproduce
# it, the rig never reached the window and the whole run is VOID, not
# green. The CRD is byte-identical between the two releases, so the only
# variable across the differential is the operator image tag.
#
# PREREQS:
#   KUBECONFIG  points at the cluster
#   SC          StorageClass for the hub PVC (default local-path)
#   PRE_TAG     operator image that predates the fix (default 1.35.1)
#   FIX_TAG     operator image under test          (default 1.36.0)
#
# KEEP=1 leaves everything standing.
set -uo pipefail

: "${KUBECONFIG:?set KUBECONFIG to the target cluster kubeconfig}"

NS="${NS:-flint-ready}"
OPNS="${OPNS:-flint-lite-system}"
SC="${SC:-local-path}"
PORT="${PORT:-2049}"
PVC_SIZE="${PVC_SIZE:-2Gi}"
OP_CHART="${CHART_SRC:-oci://registry-1.docker.io/dilipdalton/flint-lite-operator}"
CHART_VER="${CHART_VER:-0.2.8}"
PRE_TAG="${PRE_TAG:-1.35.1}"
FIX_TAG="${FIX_TAG:-1.36.0}"
HUBIMG="${HUBIMG:-dilipdalton/flint-pnfs:1.36.0}"
# How long to watch each LoadBalancer share. The window is unbounded on
# this rig, so this only has to outlast image pull + pod start.
WATCH="${WATCH:-150}"
OUT="${OUT:-/tmp/flint-ready-drill}"

export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-ready-helm-cache}"
export HELM_CONFIG_HOME="${HELM_CONFIG_HOME:-${TMPDIR:-/tmp}/flint-ready-helm-config}"
export HELM_DATA_HOME="${HELM_DATA_HOME:-${TMPDIR:-/tmp}/flint-ready-helm-data}"
mkdir -p "$HELM_CACHE_HOME" "$HELM_CONFIG_HOME" "$HELM_DATA_HOME" "$OUT" 2>/dev/null

PASSES=0; FAILURES=(); VOIDS=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
void() { echo "  ⊘ VOID: $*"; VOIDS+=("$*"); }
note() { echo "    · $*" >&2; }
fatal(){ echo "  ✗ FATAL: $*" >&2; exit 1; }

k() { kubectl -n "$NS" "$@"; }

# ---------------------------------------------------------------------------
# One sample: the share's published status, and — at the same moment —
# the two facts that make the phase MEANINGFUL. Sampling the phase alone
# is what lets a leg pass for the wrong reason.
# ---------------------------------------------------------------------------
snap() {
  local share="$1" out="$2" idx="$3" s d
  s=$(k get flintshare "$share" -o json 2>/dev/null)
  [ -z "$s" ] && return 0
  d=$(k get deploy,svc -l "flint.io/share=$share" -o json 2>/dev/null)
  [ -z "$d" ] && d='{"items":[]}'
  jq -cn --argjson s "$s" --argjson d "$d" --arg i "$idx" --arg t "$(date +%s)" '
    ($d.items // []) as $it |
    ($it | map(select(.kind=="Deployment")) | .[0]) as $dep |
    ($it | map(select(.kind=="Service"))    | .[0]) as $svc |
    {
      i:       ($i|tonumber),
      t:       ($t|tonumber),
      phase:   ($s.status.phase   // ""),
      address: ($s.status.address // ""),
      avail:   ($dep.status.availableReplicas // 0),
      svctype: ($svc.spec.type // ""),
      ingress: (($svc.status.loadBalancer.ingress // []) | length)
    }' >> "$out" 2>/dev/null
}

watch_share() {  # watch_share <share> <file> <seconds>
  local share="$1" f="$2" secs="$3" i=0
  : > "$f"
  while [ "$i" -lt "$secs" ]; do
    snap "$share" "$f" "$i"
    i=$((i+1))
    sleep 1
  done
}

cnt() { jq -s "map(select($1)) | length" "$2" 2>/dev/null; }

mkshare() {  # mkshare <name> <svctype>
  k apply -f - >/dev/null 2>&1 <<EOF
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: $1
spec:
  persistence:
    size: $PVC_SIZE
    storageClassName: $SC
  service:
    type: $2
    port: $PORT
EOF
}

cleanup() {
  set +e
  [ "${KEEP:-0}" = "1" ] && { echo; echo "KEEP=1 — namespace $NS left standing"; return; }
  k delete flintshare --all --ignore-not-found --timeout=60s >/dev/null 2>&1
  k delete pvc --all --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl delete ns "$NS" --ignore-not-found --timeout=120s >/dev/null 2>&1
}
trap cleanup EXIT

# ===========================================================================
say "setup — namespace, operator at the PRE-FIX tag $PRE_TAG"
kubectl create ns "$NS" >/dev/null 2>&1
kubectl create ns "$OPNS" >/dev/null 2>&1

helm install flint-lite-operator "$OP_CHART" --version "$CHART_VER" -n "$OPNS" \
  --set image.ref="dilipdalton/flint-lite-operator:$PRE_TAG" \
  --set hubImage="$HUBIMG" \
  --set replicas=1 --set gateway.enabled=false \
  >"$OUT/helm-install.log" 2>&1 \
  || { tail -20 "$OUT/helm-install.log"; fatal "helm install failed"; }

kubectl -n "$OPNS" rollout status deploy/flint-lite-operator --timeout=240s >/dev/null 2>&1 \
  || fatal "operator never rolled out at $PRE_TAG"

RUNNING_TAG=$(kubectl -n "$OPNS" get deploy flint-lite-operator \
  -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null)
[ "$RUNNING_TAG" = "dilipdalton/flint-lite-operator:$PRE_TAG" ] \
  || fatal "operator is running '$RUNNING_TAG', not the pre-fix tag — the differential would be meaningless"
pass "operator running $RUNNING_TAG"

# ===========================================================================
say "LEG 1 — the PRE-FIX operator must PUBLISH THE BUG (anti-vacuity)"
note "a LoadBalancer share with no address pool defined: the ingress never arrives"
mkshare lb-a LoadBalancer || fatal "FlintShare lb-a refused"
watch_share lb-a "$OUT/leg1.jsonl" "$WATCH"

L1_SAMPLES=$(wc -l < "$OUT/leg1.jsonl" | tr -d ' ')
L1_AVAIL=$(cnt '.avail>=1' "$OUT/leg1.jsonl")
L1_BUG=$(cnt '.phase=="Ready" and .address=="" and .avail>=1 and .ingress==0' "$OUT/leg1.jsonl")
echo "    samples=$L1_SAMPLES  with-available-pod=$L1_AVAIL  Ready-with-no-address=$L1_BUG"

if [ "${L1_AVAIL:-0}" -lt 1 ]; then
  void "the hub pod never became available at $PRE_TAG — the window was never entered, so nothing below distinguishes the builds"
elif [ "${L1_BUG:-0}" -lt 1 ]; then
  void "the pre-fix operator did NOT publish Ready-without-an-address — this rig does not reproduce the defect, so a green leg 2 proves nothing"
else
  pass "pre-fix operator published Ready with an empty address in $L1_BUG samples (pod available, Service has no ingress) — the window is real and the defect is present"
fi

# ===========================================================================
say "upgrading the operator to $FIX_TAG (only the image changes; the CRD is identical)"
helm upgrade flint-lite-operator "$OP_CHART" --version "$CHART_VER" -n "$OPNS" \
  --set image.ref="dilipdalton/flint-lite-operator:$FIX_TAG" \
  --set hubImage="$HUBIMG" \
  --set replicas=1 --set gateway.enabled=false \
  >"$OUT/helm-upgrade.log" 2>&1 \
  || { tail -20 "$OUT/helm-upgrade.log"; fatal "helm upgrade failed"; }
kubectl -n "$OPNS" rollout status deploy/flint-lite-operator --timeout=240s >/dev/null 2>&1 \
  || fatal "operator never rolled out at $FIX_TAG"
RUNNING_TAG=$(kubectl -n "$OPNS" get deploy flint-lite-operator \
  -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null)
[ "$RUNNING_TAG" = "dilipdalton/flint-lite-operator:$FIX_TAG" ] \
  || fatal "operator is running '$RUNNING_TAG', not the fixed tag"
pass "operator running $RUNNING_TAG"

# ===========================================================================
say "LEG 2 — the FIXED operator must never publish Ready without an address"
note "a FRESH share, so nothing is inherited from leg 1's status"
mkshare lb-b LoadBalancer || fatal "FlintShare lb-b refused"
watch_share lb-b "$OUT/leg2.jsonl" "$WATCH"

L2_SAMPLES=$(wc -l < "$OUT/leg2.jsonl" | tr -d ' ')
L2_VIOL=$(cnt '.phase=="Ready" and .address==""' "$OUT/leg2.jsonl")
# The MECHANISM guard: we must have stood in the exact window where the
# pre-fix build published Ready — pod available, no ingress, no address —
# and seen the fixed build report Starting there instead.
L2_WINDOW=$(cnt '.avail>=1 and .address=="" and .ingress==0 and .phase=="Starting"' "$OUT/leg2.jsonl")
echo "    samples=$L2_SAMPLES  Ready-with-no-address=$L2_VIOL  in-window-and-Starting=$L2_WINDOW"

if [ "${L2_WINDOW:-0}" -lt 1 ]; then
  void "never observed the fixed operator INSIDE the window (available pod, no ingress, no address) — a zero violation count here would be vacuous"
elif [ "${L2_VIOL:-0}" -ne 0 ]; then
  bad "the fixed operator published Ready with an empty address in $L2_VIOL samples"
else
  pass "0 Ready-without-address across $L2_SAMPLES samples, with $L2_WINDOW of them standing in the exact window the pre-fix build failed in (reported Starting)"
fi

# ===========================================================================
say "LEG 2b — the fixed operator must also CORRECT the share it inherited"
# lb-a was left Ready-with-no-address by the pre-fix build. A fix that only
# governs new shares would leave that lie standing.
sleep 20
A_PHASE=$(k get flintshare lb-a -o jsonpath='{.status.phase}' 2>/dev/null)
A_ADDR=$(k get flintshare lb-a -o jsonpath='{.status.address}' 2>/dev/null)
if [ "$A_PHASE" = "Ready" ] && [ -z "$A_ADDR" ]; then
  bad "lb-a is STILL Ready with no address after the upgrade — the fix does not retract a stale Ready"
elif [ -z "$A_PHASE" ]; then
  void "lb-a has no phase at all — cannot tell whether it was corrected"
else
  pass "lb-a was corrected to '$A_PHASE' (address='$A_ADDR')"
fi

# ===========================================================================
say "LEG 3 — Ready must still be REACHABLE once an address exists"
LB_MODE="simulated"
if kubectl get crd ciliumloadbalancerippools.cilium.io >/dev/null 2>&1; then
  cat <<EOF | kubectl apply -f - >/dev/null 2>&1 && LB_MODE="cilium-lb-ipam"
apiVersion: "cilium.io/v2alpha1"
kind: CiliumLoadBalancerIPPool
metadata:
  name: flint-ready-pool
spec:
  blocks:
    - cidr: "192.0.2.0/29"
EOF
fi
SVC=$(k get svc -l flint.io/share=lb-b -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
[ -n "$SVC" ] || fatal "no Service for lb-b"

if [ "$LB_MODE" = "cilium-lb-ipam" ]; then
  note "Cilium LB-IPAM present — a real controller assigns the address"
else
  note "no LB-IPAM on this cluster; writing status.loadBalancer.ingress exactly as a CCM would (SIMULATED)"
  k patch svc "$SVC" --subresource=status --type=merge \
    -p '{"status":{"loadBalancer":{"ingress":[{"ip":"192.0.2.7"}]}}}' >/dev/null 2>&1 \
    || fatal "could not write the Service status subresource"
fi

LB_IP=""
for _ in $(seq 1 30); do
  LB_IP=$(k get svc "$SVC" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null)
  [ -z "$LB_IP" ] && LB_IP=$(k get svc "$SVC" -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null)
  [ -n "$LB_IP" ] && break
  sleep 2
done
[ -n "$LB_IP" ] || fatal "the Service never got an ingress address ($LB_MODE)"
note "ingress address = $LB_IP  (via $LB_MODE)"

L3_PHASE=""; L3_ADDR=""
for _ in $(seq 1 45); do
  L3_PHASE=$(k get flintshare lb-b -o jsonpath='{.status.phase}' 2>/dev/null)
  L3_ADDR=$(k get flintshare lb-b -o jsonpath='{.status.address}' 2>/dev/null)
  [ "$L3_PHASE" = "Ready" ] && [ -n "$L3_ADDR" ] && break
  sleep 2
done
# The mechanism guard: the address must be the LB's, not a fallback to
# the .svc name. A non-empty address is not enough.
if [ "$L3_ADDR" = "$LB_IP:$PORT" ] && [ "$L3_PHASE" = "Ready" ]; then
  pass "lb-b went Ready with address '$L3_ADDR' — read from the LoadBalancer ingress, so the fix cannot wedge a share ($LB_MODE)"
elif [ "$L3_PHASE" = "Ready" ]; then
  bad "lb-b is Ready but address='$L3_ADDR', expected '$LB_IP:$PORT' — Ready was reached by some other path"
else
  bad "lb-b never reached Ready after the address appeared (phase='$L3_PHASE', address='$L3_ADDR') — the fix WEDGED the share"
fi

# ===========================================================================
say "LEG 4 — the default ClusterIP path must be unaffected"
mkshare cip ClusterIP || fatal "FlintShare cip refused"
C_PHASE=""; C_ADDR=""; C_T0=$(date +%s); C_READY_AT=""
for _ in $(seq 1 90); do
  C_PHASE=$(k get flintshare cip -o jsonpath='{.status.phase}' 2>/dev/null)
  C_ADDR=$(k get flintshare cip -o jsonpath='{.status.address}' 2>/dev/null)
  if [ "$C_PHASE" = "Ready" ]; then C_READY_AT=$(date +%s); break; fi
  sleep 2
done
CSVC=$(k get svc -l flint.io/share=cip -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
WANT="$CSVC.$NS.svc.cluster.local:$PORT"
if [ "$C_PHASE" = "Ready" ] && [ "$C_ADDR" = "$WANT" ]; then
  pass "ClusterIP share Ready in $((C_READY_AT-C_T0))s with address '$C_ADDR' (the DNS form, so it took the ClusterIP branch)"
elif [ "$C_PHASE" = "Ready" ]; then
  bad "ClusterIP share Ready but address='$C_ADDR', expected '$WANT'"
else
  bad "ClusterIP share never reached Ready (phase='$C_PHASE') — the fix broke the default path"
fi

# ===========================================================================
say "LEG 5 — the published $FIX_TAG hub actually serves NFS"
# Recipe A, static PV. The PV MUST carry the ClusterIP, not the .svc
# name: kubelet resolves nfs.server on the NODE, which has no cluster
# DNS, and a name there hangs the mount with ZERO events.
CIP=$(k get svc "$CSVC" -o jsonpath='{.spec.clusterIP}' 2>/dev/null)
if [ -z "$CIP" ]; then
  void "no ClusterIP for $CSVC — cannot build the consumer PV"
else
  kubectl apply -f - >/dev/null 2>&1 <<EOF
apiVersion: v1
kind: PersistentVolume
metadata:
  name: flint-ready-pv
spec:
  capacity: { storage: 1Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  nfs: { server: "$CIP", path: "/" }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: flint-ready-pvc
  namespace: $NS
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: flint-ready-pv
  resources: { requests: { storage: 1Gi } }
EOF
  k apply -f - >/dev/null 2>&1 <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: consumer
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: busybox:1.36
      command: ["sh","-c","echo flint-ready-$FIX_TAG > /mnt/probe.txt && cat /mnt/probe.txt && sync"]
      volumeMounts: [{ name: v, mountPath: /mnt }]
  volumes:
    - name: v
      persistentVolumeClaim: { claimName: flint-ready-pvc }
EOF
  CPHASE=""
  for _ in $(seq 1 60); do
    CPHASE=$(k get pod consumer -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$CPHASE" = "Succeeded" ] || [ "$CPHASE" = "Failed" ] && break
    sleep 3
  done
  LOGS=$(k logs consumer 2>/dev/null | tr -d '\r')
  if [ "$CPHASE" = "Succeeded" ] && [ "$LOGS" = "flint-ready-$FIX_TAG" ]; then
    pass "consumer mounted the $FIX_TAG hub over NFS and read back what it wrote"
  else
    bad "consumer pod phase='$CPHASE', read back '$LOGS' (expected 'flint-ready-$FIX_TAG')"
    k describe pod consumer 2>/dev/null | tail -20
  fi
fi

# ===========================================================================
say "RESULT"
echo "  passes:   $PASSES"
echo "  failures: ${#FAILURES[@]}"
echo "  voids:    ${#VOIDS[@]}"
for f in ${FAILURES+"${FAILURES[@]}"}; do echo "    ✗ $f"; done
for v in ${VOIDS+"${VOIDS[@]}"};    do echo "    ⊘ $v"; done
echo "  samples kept in $OUT"
if [ "${#FAILURES[@]}" -ne 0 ]; then echo "DRILL FAILED"; exit 1; fi
if [ "${#VOIDS[@]}" -ne 0 ];    then echo "DRILL VOID — it did not prove what it set out to prove"; exit 3; fi
echo "DRILL PASSED"
