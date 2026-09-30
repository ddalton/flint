#!/bin/bash
# nfs-proxy step 5, the last scale prerequisite (design §7a): operator and
# proxy MEMORY with N FlintShares, 20,000 by default, on kind (the box).
#
# The shares are created already hibernated (the idle-state annotation,
# a bucket, no disk), so the operator parks each as its CR alone and
# nothing else is created: no pods, no hub objects. What is measured is
# what 20,000 CRs cost the processes that hold the fleet in a reflector —
# the fixed cost the §7a extrapolation (~17 KB/share of managedFields,
# ~350 MB at 20,000, against a 256Mi-then-512Mi limit) was about.
# Limits are raised so the peak is SEEN rather than an OOMKill.
#   N=20000 bash step5-scale-kind.sh        # KEEP=1 leaves the cluster up
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CHART=$REPO/flint-lite-operator-chart
CLUSTER=flint-scale
OPNS=flint-system; NS=fleet
TAG=${TAG:-step5-dev}
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
N=${N:-20000}; BATCH=1000
OUT=$HOME/nfs-proxy-scale-$N
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp
rm -rf $OUT; mkdir -p $OUT $TMPDIR
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
K() { kubectl "$@"; }
cleanup() { [ "${KEEP:-0}" = 1 ] || kind delete cluster --name $CLUSTER >/dev/null 2>&1; }
trap cleanup EXIT INT TERM
for i in $OPIMG $HUBIMG; do docker image inspect $i >/dev/null 2>&1 || { echo "missing $i (build it with step5-kind.sh first)"; exit 1; }; done

echo "== cluster"
kind delete cluster --name $CLUSTER >/dev/null 2>&1
kind create cluster --name $CLUSTER --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
for i in $OPIMG $HUBIMG; do kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || exit 1; done
K create ns $OPNS >/dev/null; K create ns $NS >/dev/null
K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=x --from-literal=AWS_SECRET_ACCESS_KEY=y >/dev/null
cat > $OUT/values.yaml <<EOF
image: { ref: "$OPIMG", pullPolicy: Never }
hubImage: "$HUBIMG"
hubImagePullPolicy: Never
replicas: 1
resources: { requests: { cpu: 100m, memory: 128Mi }, limits: { memory: 4Gi } }
restartOnTgtRestart: { enabled: false }
nfsProxy:
  enabled: true
  service: { type: NodePort }
  resources: { requests: { cpu: 100m, memory: 128Mi }, limits: { memory: 4Gi } }
  identities: [{ name: all, sources: ["0.0.0.0/0"], workspaces: ["*"] }]
EOF
helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || { echo "operator not ready"; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || { echo "proxy not ready"; exit 1; }

# VmRSS / VmHWM (peak) of a Deployment's main process, in MiB.
mem() { K -n $OPNS exec deploy/$1 -- sh -c 'grep -E "^Vm(RSS|HWM)" /proc/1/status' 2>/dev/null | awk '{printf "%s=%dMi ", $1, $2/1024}'; }
restarts() { K -n $OPNS get pod -l app.kubernetes.io/name=$1 -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}'; }
echo "empty fleet: operator [$(mem flint-lite-operator)] proxy [$(mem flint-lite-operator-nfs-proxy)]" | tee $OUT/mem.txt

echo "== seeding $N pre-hibernated shares"
T0=$(date +%s); SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
for ((b = 0; b < N; b += BATCH)); do
  python3 - $b $BATCH $N $NS $SINCE > $OUT/batch.yaml <<'P'
import sys
b, batch, n, ns, since = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5]
for i in range(b, min(b + batch, n)):
    print(f"""---
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata:
  name: p-{i:05d}
  namespace: {ns}
  labels: {{ chert.us/project-id: p-{i:05d} }}
  annotations: {{ chert.us/idle-state: Hibernated, chert.us/idle-since: "{since}" }}
spec:
  bucket: fleet
  keyPrefix: p-{i:05d}/
  endpoint: http://s3.invalid:9000
  region: us-east-1
  credentialsSecretRef: s3
  persistence: {{ size: 1Gi }}""")
P
  K create -f $OUT/batch.yaml >/dev/null 2>>$OUT/seed.err || { echo "seeding failed at $b"; tail -3 $OUT/seed.err; exit 1; }
done
echo "seeded in $(( $(date +%s) - T0 ))s"
count() { K -n $NS get flintshares --no-headers 2>/dev/null | wc -l; }
parked() { K -n $NS get flintshares -o jsonpath='{range .items[*]}{.status.phase}{"\n"}{end}' 2>/dev/null | grep -c '^Hibernated$'; }
check "all $N shares exist" '[ "$(count)" = $N ]'

echo "== waiting for the operator to park every share"
T1=$(date +%s); P=0
while [ $(( $(date +%s) - T1 )) -lt 3600 ]; do
  P=$(parked); echo "  $(( $(date +%s) - T1 ))s: parked $P / $N; operator [$(mem flint-lite-operator)] proxy [$(mem flint-lite-operator-nfs-proxy)]" | tee -a $OUT/mem.txt
  [ "$P" = $N ] && break
  sleep 60
done
PARK_S=$(( $(date +%s) - T1 ))
check "the operator reported every share Hibernated (within ${PARK_S}s)" '[ "$P" = $N ]'
check "CR-only: no Deployment, Service or ConfigMap in the fleet namespace" '[ "$(K -n $NS get deploy,svc,cm --no-headers 2>/dev/null | grep -vc kube-root-ca)" = 0 ]'
check "the operator did not restart (no OOMKill)" '[ "$(restarts flint-lite-operator)" = 0 ]'
check "the proxy did not restart (no OOMKill)" '[ "$(restarts flint-lite-operator-nfs-proxy)" = 0 ]'
sleep 120
echo "settled: operator [$(mem flint-lite-operator)] proxy [$(mem flint-lite-operator-nfs-proxy)]" | tee -a $OUT/mem.txt
# What one stored share costs, and how much of it is managedFields.
# --show-managed-fields: kubectl hides them otherwise (run 1 reported 0%).
K -n $NS get flintshare p-00000 -o json --show-managed-fields > $OUT/one.json
python3 - $OUT/one.json <<'P' | tee -a $OUT/mem.txt
import json, sys
o = json.load(open(sys.argv[1]))
full = len(json.dumps(o))
mf = len(json.dumps(o["metadata"].get("managedFields", [])))
print(f"one share as JSON: {full} bytes, of which managedFields {mf} ({100*mf//max(full,1)}%)")
P
echo "RESULT: $ok passed, $bad failed"
