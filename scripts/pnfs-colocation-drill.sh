#!/usr/bin/env bash
# IS THE 2432 FAST MODE JUST CLIENT/DS CO-LOCATION?
#
#   ./scripts/pnfs-colocation-drill.sh <ds-node> <remote-node> [pvc]
#
# runbb proved the 918 steady state is the WireGuard tunnel ceiling for
# REMOTE reads. The surviving explanation for the 2432 fast mode is that
# the consumer pod sat ON the DS node (no NIC, no WG, no tunnel — pure
# same-node veth + page cache). runba enforced client!=DS and never saw
# fast mode once in 15 reads; the runaz sweep never controlled placement.
#
# Protocol: ONE volume, ONE layout. Arm R (remote, control) must
# reproduce the known steady state; arm L (co-located) tests the
# hypothesis; a final R re-baseline guards drift. The pod moves between
# arms; the volume does not.
set -uo pipefail
DSNODE=${1:?usage: pnfs-colocation-drill.sh <ds-node> <remote-node> [pvc]}
REMOTE=${2:?}
PVC=${3:-coloc}
NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-/tmp/pnfs-coloc}
SHARDS=${SHARDS:-4}
SHARD_GIB=${SHARD_GIB:-2}
WORKERS=${WORKERS:-4}
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

ts() { date +%H:%M:%S; }
say() { printf "[%s] %s\n" "$(ts)" "$*"; }
die() { printf "[%s] ✗ ABORT: %s\n" "$(ts)" "$*"; exit 1; }

DS_PODS=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' 2>/dev/null | sort -u)
[ "$(echo "$DS_PODS" | grep -c .)" = "1" ] || die "need exactly 1 DS, have: $DS_PODS"
echo "$DS_PODS" | grep -qx "$DSNODE" || die "DS is on '$DS_PODS', not '$DSNODE'"
[ "$DSNODE" != "$REMOTE" ] || die "ds-node and remote-node must differ"
say "✓ one DS on $DSNODE; control node $REMOTE"

kubectl create configmap cm-coloc --from-file=bench.py="$HERE/pnfs-model-bench.py" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl get pvc "$PVC" >/dev/null 2>&1 || cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: $PVC}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ${SC:-flint-pnfs}
  resources: {requests: {storage: 64Gi}}
YAML

pod_on() {  # node
  kubectl delete pod coloc --ignore-not-found --wait=true >/dev/null 2>&1
  cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: coloc}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $1}
  containers:
  - name: b
    image: python:3.12-alpine
    command: ["sh","-c","sleep 100000"]
    volumeMounts: [{name: d, mountPath: /data}, {name: s, mountPath: /bench}]
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: $PVC}
  - name: s
    configMap: {name: cm-coloc}
YAML
  kubectl wait --for=condition=Ready pod/coloc --timeout=300s >/dev/null 2>&1 \
    || die "pod not Ready on $1"
  # Trust nothing: assert where it actually landed.
  local landed
  landed=$(kubectl get pod coloc -o jsonpath='{.spec.nodeName}')
  [ "$landed" = "$1" ] || die "pod landed on $landed, wanted $1"
  say "✓ consumer pod on $1"
}

one_read() {  # arm n node
  local arm=$1 n=$2 node=$3 res mibps
  "$HERE/nodesh.sh" "$node" 'sync; echo 3 > /proc/sys/vm/drop_caches' >/dev/null 2>&1
  res=$(kubectl exec coloc -- python3 /bench/bench.py read --dir /data \
        --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream \
        --workers "$WORKERS" 2>&1 | grep RESULT)
  mibps=$(sed -n 's/.*mibps=\([0-9]*\).*/\1/p' <<<"$res")
  [ -n "${mibps:-}" ] || die "read $arm/$n produced no RESULT"
  echo "$arm $n $mibps" >> "$OUT/points.txt"
  say "  $arm #$n: $mibps MiB/s"
}

: > "$OUT/points.txt"

say "── arm R (remote control, expect the known steady state) ──"
pod_on "$REMOTE"
if ! kubectl exec coloc -- sh -c 'ls /data/model-*.safetensors >/dev/null 2>&1'; then
  say "laying out ${SHARDS}x${SHARD_GIB}GiB (once)"
  kubectl exec coloc -- python3 /bench/bench.py write --dir /data \
    --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream --workers "$WORKERS" \
    2>&1 | grep RESULT || die "layout failed"
fi
for i in 1 2 3; do one_read R "$i" "$REMOTE"; done

say "── arm L (co-located with the DS) ──"
pod_on "$DSNODE"
for i in 1 2 3 4 5; do one_read L "$i" "$DSNODE"; done

say "── arm R re-baseline (window guard) ──"
pod_on "$REMOTE"
for i in 4 5; do one_read R "$i" "$REMOTE"; done

python3 - "$OUT/points.txt" <<'PY'
import sys, statistics as st
rows=[l.split() for l in open(sys.argv[1]) if l.strip()]
R=[int(m) for a,n,m in rows if a=='R']
L=[int(m) for a,n,m in rows if a=='L']
print()
print(f"R (remote):    {sorted(R)}  median {st.median(R):.0f} MiB/s")
print(f"L (co-located): {sorted(L)}  median {st.median(L):.0f} MiB/s")
drift = max(R[0], R[-1]) / max(1, min(R[0], R[-1]))
print(f"R first/last drift {drift:.2f}x")
if drift > 1.4:
    print("*** WINDOW DID NOT HOLD — VOID ***"); sys.exit(0)
ratio = st.median(L) / st.median(R)
print(f"co-location ratio: {ratio:.2f}x")
if st.median(L) > 1800 and ratio > 1.8:
    print("VERDICT: CONFIRMED — co-location reproduces the fast mode "
          f"({st.median(L):.0f} MiB/s vs remote {st.median(R):.0f})")
elif ratio < 1.2:
    print("VERDICT: REFUTED — co-location does NOT reproduce the fast mode; "
          "the 2432 needs another explanation")
else:
    print("VERDICT: PARTIAL — co-location helps but does not reach the "
          "historical 2432-2495 band; more than placement is involved")
PY
kubectl delete pod coloc --ignore-not-found --wait=false >/dev/null 2>&1
kubectl delete configmap cm-coloc --ignore-not-found >/dev/null 2>&1
say "raw: $OUT/points.txt"
