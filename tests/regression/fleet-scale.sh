#!/bin/bash
# ---------------------------------------------------------------------------
# Rig A — the CONTROL PLANE at the design target.
#
#   tests/regression/fleet-scale.sh <shares> <live> [storage-class]
#   tests/regression/fleet-scale.sh 3000 300 local-path
#
# WHAT THIS MEASURES, AND WHAT IT DELIBERATELY DOES NOT
#
# The operator's design target is 3000 FlintShares with ~300 live hubs.
# Nothing has ever asserted it; every drill has run 2-4 shares. What
# breaks first is the CONTROL PLANE — reconcile rate, apiserver writes,
# operator RSS, arbitration CPU — so the live shares here run
# `flint-hub-stub`, which serves the operator's three touchpoints (TCP
# 2049, /health, /status) in a few MB. That is what lets 300 "live"
# shares fit on a small rig.
#
# It therefore says NOTHING about the data plane: no state.db, no tier,
# no S3, no real PVC I/O. Per-hub constants belong to Rig B, on 10-30
# REAL hubs, extrapolated from there. Do not quote a number from this
# script as a data-plane result.
#
# ANTI-VACUITY IS THE WHOLE PROBLEM WITH A LOAD TEST
#
# A rig that stands up 3000 CRs and reports "fine" is the easiest false
# pass available: if the operator crash-looped, or every share read as
# unreachable, or the seeder silently created 30 objects instead of
# 3000, every rate would look wonderful. So every oracle below is
# paired with a guard that fails when the rig itself stopped working.
# ---------------------------------------------------------------------------
set -uo pipefail

N=${1:?usage: fleet-scale.sh <shares> <live> [storage-class]}
LIVE=${2:?usage: fleet-scale.sh <shares> <live> [storage-class]}
SC=${3:-local-path}
NS_COUNT=${NS_COUNT:-10}
OPNS=${OPNS:-flint-lite-system}
STUB_IMAGE=${STUB_IMAGE:-dilipdalton/flint-hub-stub:1.32.0}
OUT=${OUT:-/tmp/fleet-scale-$$}
mkdir -p "$OUT"

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; FAILED=$((FAILED+1)); }
FAILED=0

say "rig: $N shares, $LIVE live (stub hubs), $NS_COUNT namespaces, sc=$SC"
echo "  output: $OUT"

# RESOURCES ARE SET EXPLICITLY, AND SMALL, ON PURPOSE. A stub is not a
# hub: it holds a socket and serves a JSON document. The operator's
# fleet default (100m/128Mi) is sized for a real NFS server, and at 300
# live that is 30 vCPU of REQUESTS — more than this rig's four workers
# have, so 162 of 300 pods sat Unschedulable on the first attempt and
# the run would have measured cluster capacity rather than the control
# plane. Worth knowing as a deployment fact in its own right: making
# hubs schedulable makes the capacity they were silently borrowing
# visible, and 300 live hubs need planning for.

# THE LADDER IS ARMED ON PURPOSE. poll_hub runs from the idle
# evaluation, so a share with no spec.idle is NEVER polled - and the
# per-share /status poll is one of the terms this rig exists to
# measure. Thresholds sit far outside the run (suspend 1h, hibernate
# 24h) so the ladder is exercised without moving anything mid-window.

# --- namespaces ------------------------------------------------------------
for i in $(seq 0 $((NS_COUNT-1))); do
    kubectl create ns "fleet-$i" >/dev/null 2>&1
done

# --- credentials the CRD requires for a tiered share ------------------------
for i in $(seq 0 $((NS_COUNT-1))); do
    kubectl -n "fleet-$i" create secret generic rig-creds \
        --from-literal=AWS_ACCESS_KEY_ID=rig \
        --from-literal=AWS_SECRET_ACCESS_KEY=rig >/dev/null 2>&1
done

# --- seed ------------------------------------------------------------------
# Parked shares are pre-stamped with the operator's OWN durable carrier
# (flint.io/idle-state + idle-since), so they cost zero pods from birth.
# That is legitimate rather than a cheat: those annotations ARE how the
# operator persists the rung, and `decide()` returns Hold for a share
# that is down and unrequested.
say "seeding $N shares in batches"
SEED_T0=$(date +%s)
BATCH=200
IDLE_SINCE=$(date -u -v-2H +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -d '2 hours ago' +%Y-%m-%dT%H:%M:%SZ)
i=0
while [ $i -lt "$N" ]; do
    f="$OUT/batch-$i.yaml"
    : > "$f"
    for j in $(seq $i $((i+BATCH-1))); do
        [ "$j" -ge "$N" ] && break
        ns="fleet-$((j % NS_COUNT))"
        if [ "$j" -lt "$LIVE" ]; then
            cat >> "$f" <<YAML
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: s$j
  namespace: $ns
spec:
  bucket: rig-bucket
  keyPrefix: "t$(printf '%05d' $j)/"
  region: us-west-1
  credentialsSecretRef: rig-creds
  image: $STUB_IMAGE
  persistence: {size: 1Gi, storageClassName: $SC}
  monitoring: {enabled: true, port: 8080}
  idle: {suspendAfterSecs: 3600, hibernateAfterSecs: 86400}
  resources: {requests: {cpu: 5m, memory: 16Mi}}
---
YAML
        else
            cat >> "$f" <<YAML
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: s$j
  namespace: $ns
  annotations:
    flint.io/idle-state: Suspended
    flint.io/idle-since: "$IDLE_SINCE"
spec:
  bucket: rig-bucket
  keyPrefix: "t$(printf '%05d' $j)/"
  region: us-west-1
  credentialsSecretRef: rig-creds
  image: $STUB_IMAGE
  persistence: {size: 1Gi, storageClassName: $SC}
  monitoring: {enabled: true, port: 8080}
  idle: {suspendAfterSecs: 3600, hibernateAfterSecs: 86400}
  resources: {requests: {cpu: 5m, memory: 16Mi}}
---
YAML
        fi
    done
    kubectl apply -f "$f" >/dev/null 2>&1 &
    i=$((i+BATCH))
done
wait
SEED_T1=$(date +%s)
echo "  seeded in $((SEED_T1-SEED_T0))s"

# --- ANTI-VACUITY 1: the CRs must actually exist ---------------------------
say "[A1] the seeder really created the fleet"
ACTUAL=$(kubectl get flintshares -A --no-headers 2>/dev/null | wc -l | tr -d ' ')
echo "  flintshares present = $ACTUAL (want $N)"
if [ "$ACTUAL" -lt $((N * 95 / 100)) ]; then
    fail "only $ACTUAL of $N shares exist — every rate below would be measuring a SMALLER fleet"
    echo "RIG IS VACUOUS, stopping" >&2; exit 1
else
    pass "$ACTUAL shares"
fi

# --- converge --------------------------------------------------------------
say "waiting for the fleet to settle (live shares Ready, parked at 0 pods)"
CONV_T0=$(date +%s)
for t in $(seq 1 120); do
    READY=$(kubectl get flintshares -A -o jsonpath='{range .items[*]}{.status.phase}{"\n"}{end}' 2>/dev/null | grep -c '^Ready$')
    PODS=$(kubectl get pods -A -l app.kubernetes.io/name=flint-lite --no-headers 2>/dev/null | wc -l | tr -d ' ')
    [ $((t % 6)) = 0 ] && echo "  t+$((t*10))s ready=$READY pods=$PODS"
    [ "$READY" -ge "$LIVE" ] && break
    sleep 10
done
CONV_T1=$(date +%s)
CONVERGE=$((CONV_T1-CONV_T0))
echo "  converged (or gave up) after ${CONVERGE}s: ready=$READY pods=$PODS"

# --- ANTI-VACUITY 2: the operator must be ALIVE, not crash-looping ---------
say "[A2] the operator survived the fleet"
OPPODS=$(kubectl -n "$OPNS" get pods -l app.kubernetes.io/name=flint-lite-operator --no-headers 2>/dev/null)
echo "$OPPODS" | sed 's/^/    /'
RESTARTS=$(echo "$OPPODS" | awk '{s+=$4} END{print s+0}')
RUNNING=$(echo "$OPPODS" | grep -c "Running")
if [ "$RUNNING" -lt 1 ]; then
    fail "no operator pod is Running — every number below is from a dead control plane"
    echo "RIG IS VACUOUS, stopping" >&2; exit 1
fi
if [ "$RESTARTS" -gt 0 ]; then
    fail "operator restarted $RESTARTS time(s) — OOMKill or panic under fleet load (THIS IS A RESULT, record it)"
else
    pass "operator stable, 0 restarts"
fi

# --- ANTI-VACUITY 3: the live shares must actually be REACHED --------------
# If the stub's document drifted, poll_hub fails, every share reads
# unreachable, and the fleet looks beautifully quiet while measuring
# nothing. HubReachable is the guard.
say "[A3] the operator can actually read the stub hubs"
REACH=$(kubectl get flintshares -A -o json 2>/dev/null | python3 -c "
import json,sys
d=json.load(sys.stdin)
ok=bad=0
for it in d.get('items',[]):
    for c in (it.get('status',{}) or {}).get('conditions',[]) or []:
        if c.get('type')=='HubReachable':
            if c.get('status')=='True': ok+=1
            else: bad+=1
print(ok,bad)
")
echo "  HubReachable true/false = $REACH"
RO=$(echo "$REACH" | awk '{print $1}')
if [ "${RO:-0}" -lt $((LIVE / 2)) ]; then
    fail "only ${RO:-0} live shares are reachable of $LIVE — the stub is not being polled, so the poll term is UNMEASURED"
else
    pass "${RO} live shares reachable"
fi

# --- the measurement -------------------------------------------------------
# Read from the APISERVER's own metrics, filtered to the operator's
# user-agent, rather than trusting anything the operator says about
# itself. Two samples, WINDOW apart, and the rate is the difference.
# THE WINDOW MUST EXCEED REQUEUE_SETTLED (300s) or the measurement
# lands in the quiet gap between requeues and reports a rate of ~0 for a
# fleet that is working normally. Caught by A4 on the first smoke run.
WINDOW=${WINDOW:-400}
if [ "$WINDOW" -lt 320 ]; then
    echo "  ! WINDOW=$WINDOW is shorter than the 300s settled requeue — the rate will be an artifact" >&2
fi
say "measuring for ${WINDOW}s of steady state"

# apiserver_request_total carries NO client/user-agent label - its
# labels are code/component/dry_run/group/resource/scope/subresource/
# verb/version. So attribution is by RESOURCE: flintshares traffic is
# the operator's by construction (nothing else touches the CRD), and it
# is where the status-write term lives - the one that turns a large
# fleet into constant apiserver load. Child-object traffic is reported
# separately and is NOT claimed to be the operator's alone.
apiserver_ops() {
    kubectl get --raw /metrics 2>/dev/null | awk '
        /^apiserver_request_total\{/ {
            r=""; v=""
            if (match($0, /resource="[^"]*"/)) { r=substr($0, RSTART+10, RLENGTH-11) }
            if (match($0, /verb="[^"]*"/))     { v=substr($0, RSTART+6,  RLENGTH-7)  }
            n=$NF+0
            if (r=="flintshares") { fs[v]+=n; fsall+=n }
            if (r=="deployments"||r=="services"||r=="configmaps"||r=="persistentvolumeclaims") { chall+=n }
            all+=n
        }
        END {
            for (v in fs) printf "FS_%s=%d\n", v, fs[v]
            printf "FS_TOTAL=%d\nCHILD_TOTAL=%d\nTOTAL=%d\n", fsall, chall, all
        }'
}
oprss() {
    kubectl -n "$OPNS" top pod --no-headers 2>/dev/null | awk '{c+=$2+0; m+=$3+0} END{print c" "m}'
}

apiserver_ops > "$OUT/api-t0.txt"
RSS0=$(oprss)
T0=$(date +%s)
sleep "$WINDOW"
apiserver_ops > "$OUT/api-t1.txt"
RSS1=$(oprss)
T1=$(date +%s)
ELAPSED=$((T1-T0))

say "RESULTS — $N shares, $LIVE live, ${ELAPSED}s window"
python3 - "$OUT/api-t0.txt" "$OUT/api-t1.txt" "$ELAPSED" "$N" "$LIVE" <<'PY'
import sys
a,b,el,n,live = sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
def load(p):
    d={}
    for l in open(p):
        if '=' in l:
            k,v=l.strip().split('=',1); d[k]=int(v)
    return d
x,y=load(a),load(b)
def rate(k): return (y.get(k,0)-x.get(k,0))/el
print(f"  FLINTSHARES traffic - the operator's, by construction (window {el}s):")
fs_w=fs_r=0.0
for k in sorted(set(x)|set(y)):
    if not k.startswith('FS_') or k=='FS_TOTAL': continue
    r=rate(k)
    if r<=0: continue
    verb=k[3:]
    print(f"    {verb:<12} {r:8.2f}/s")
    # APPLY is server-side apply — a WRITE. Classifying it as a read
    # made the status-write term read 0.00/s on the first real run,
    # against 19.5 APPLY/s actually happening.
    if verb in ('POST','PUT','PATCH','DELETE','APPLY'): fs_w+=r
    else: fs_r+=r
print(f"    {'TOTAL':<12} {rate('FS_TOTAL'):8.2f}/s   (writes {fs_w:.2f}/s, reads {fs_r:.2f}/s)")
print()
print(f"  child objects (deploy/svc/cm/pvc, not the operator's alone): {rate('CHILD_TOTAL'):.2f}/s")
print(f"  whole apiserver:                                             {rate('TOTAL'):.2f}/s")
print()
print(f"  per share: {rate('FS_TOTAL')/n*60:.2f} req/min/share, {fs_w/n*60:.2f} writes/min/share")
print(f"  STATUS-WRITE LOAD: {fs_w:.2f}/s across {n} shares with NOTHING changing")
PY
echo "  operator cpu(m) mem(Mi):  before [$RSS0]  after [$RSS1]"

# --- ANTI-VACUITY 4: the operator must be DOING something ------------------
say "[A4] the measurement window saw real work"
TOTDELTA=$(python3 -c "
import sys
def load(p):
    d={}
    for l in open(p):
        if '=' in l:
            k,v=l.strip().split('=',1); d[k]=int(v)
    return d
x=load('$OUT/api-t0.txt'); y=load('$OUT/api-t1.txt')
print(y.get('FS_TOTAL',0)-x.get('FS_TOTAL',0))")
echo "  flintshare apiserver requests in the window = $TOTDELTA"
# Expect at least half of one full requeue cycle's worth of traffic:
# N shares / 300s * window, halved for slack. A flat threshold passes
# vacuously on a big fleet and fails spuriously on a small one.
EXPECT=$(( N * WINDOW / 300 / 2 ))
[ "$EXPECT" -lt 5 ] && EXPECT=5
echo "  expected at least $EXPECT (N/300s x window, halved)"
if [ "${TOTDELTA:-0}" -lt "$EXPECT" ]; then
    fail "the operator made ~no apiserver calls — either it is wedged, or the user-agent filter matched nothing, so the RATE ABOVE IS NOT A RATE"
else
    pass "the operator was live and working during the window"
fi

echo
if [ "$FAILED" -gt 0 ]; then
    echo "FLEET RIG: $FAILED guard(s) failed — read them before quoting any number above"
    exit 1
fi
echo "FLEET RIG: all guards passed; the numbers above are measurements of a live fleet"
