#!/usr/bin/env bash
# WHERE IN THE DRAIN CHAIN DOES A pNFS READ ACTUALLY BIND?
#
#   ./scripts/pnfs-flow-classification.sh <client-node> <ds-node> [pvc]
#
# THE QUESTION. The 2026-08-03 adversarial research pass upheld the
# LOCALIZATION (the read constraint is downstream of DS dispatch, in the
# DS-send → network → client-receive chain) but REFUTED the named
# mechanism: "918 = 4 flows × 230 at the AWS ~5 Gbps/flow cap" was
# arithmetic — per-flow throughput has NEVER been measured on any rig.
# The ss instrument in pnfs-inflight-experiment.sh was defined and never
# invoked. This script exists to run it, properly, at 1 Hz, on BOTH
# endpoints, and to classify the limiter per flow-second.
#
# ONE CAPTURE DISCRIMINATES FOUR CANDIDATES:
#   delivery_rate >> achieved, flows idle  -> app-limited (flint DS send)
#   sndbuf_limited dominant                -> DS socket configuration
#   cwnd small + retransmits               -> fabric / path property
#   rwnd_limited or client softirq pegged  -> client receive path
#
# INSTRUMENT DISCIPLINE (eight self-reporting instrument bugs this
# campaign): every meter proves itself on a throwaway read before any
# measured window; samplers dump RAW ss output (parsing happens off-node,
# where a regex bug is visible, not silent); the analyzer refuses flows
# whose counters never move.
set -uo pipefail
CLIENT=${1:?usage: pnfs-flow-classification.sh <client-node> <ds-node> [pvc]}
DSNODE=${2:?}
PVC=${3:-flowclass}
NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-/tmp/pnfs-flowclass}
SHARDS=${SHARDS:-4}
SHARD_GIB=${SHARD_GIB:-2}
WORKERS=${WORKERS:-4}
N_READS=${N_READS:-10}
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

ts() { date +%H:%M:%S; }
say() { printf "[%s] %s\n" "$(ts)" "$*"; }
die() { printf "[%s] ✗ ABORT: %s\n" "$(ts)" "$*"; exit 1; }

say "pNFS flow classification — client=$CLIENT ds=$DSNODE pvc=$PVC out=$OUT"

# ── preconditions (each earned its place on a prior rig) ────────────────
DS_PODS=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' 2>/dev/null | sort -u)
DS_COUNT=$(echo "$DS_PODS" | grep -c . || true)
[ "$DS_COUNT" = "1" ] || die "expected exactly 1 data server, found $DS_COUNT ($DS_PODS)"
echo "$DS_PODS" | grep -qx "$DSNODE" || die "the DS is on '$DS_PODS', not '$DSNODE'"
echo "$DS_PODS" | grep -qx "$CLIENT" && die "client $CLIENT also hosts the DS"
DS_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds -o jsonpath='{.items[0].metadata.name}')
DS_POD_IP=$(kubectl get pod -n "$NS" "$DS_POD" -o jsonpath='{.status.podIP}')
DS_PID=$("$HERE/nodesh.sh" "$DSNODE" 'pgrep -f flint-pnfs-ds | head -1' 2>/dev/null | tail -1)
[ -n "${DS_PID:-}" ] || die "no flint-pnfs-ds process on $DSNODE"
CLIENT_IP=$(kubectl get node "$CLIENT" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
DSNODE_IP=$(kubectl get node "$DSNODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
say "✓ one DS ($DS_POD pid=$DS_PID ip=$DS_POD_IP on $DSNODE_IP); client node ip $CLIENT_IP"
kubectl get svc -n "$NS" -o wide > "$OUT/services.txt" 2>/dev/null

# ── consumer pod + layout (runba shape) ─────────────────────────────────
POD="flowclass"
kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
kubectl create configmap cm-flowclass --from-file=bench.py="$HERE/pnfs-model-bench.py" \
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
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $POD}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  containers:
  - name: b
    image: python:3.12-alpine
    command: ["sh","-c","sleep 100000"]
    volumeMounts: [{name: d, mountPath: /data}, {name: s, mountPath: /bench}]
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: $PVC}
  - name: s
    configMap: {name: cm-flowclass}
YAML
kubectl wait --for=condition=Ready "pod/$POD" --timeout=300s >/dev/null 2>&1 \
  || die "consumer pod not Ready"
say "✓ consumer pod ready on $CLIENT"

if ! kubectl exec "$POD" -- sh -c 'ls /data/model-*.safetensors >/dev/null 2>&1'; then
  say "laying out ${SHARDS}x${SHARD_GIB}GiB (once, outside every window)"
  kubectl exec "$POD" -- python3 /bench/bench.py write --dir /data \
    --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream --workers "$WORKERS" \
    2>&1 | grep RESULT || die "layout write failed"
fi

drop_client_cache() {
  "$HERE/nodesh.sh" "$CLIENT" 'sync; echo 3 > /proc/sys/vm/drop_caches' >/dev/null 2>&1
}

# ── samplers: 1 Hz raw ss dumps, controlled by a marker file ────────────
# Client side: the kernel NFS client lives in the HOST netns; NFS flows
# are dport 2049 (MDS service + per-DS service). Raw output travels home;
# parsing happens on the Mac where a bug is visible.
start_client_sampler() {  # tag
  "$HERE/nodesh.sh" "$CLIENT" - <<EOS >/dev/null 2>&1
touch /tmp/flowsample.on
nohup sh -c 'while [ -e /tmp/flowsample.on ]; do
  echo "T \$(date +%s.%N)"
  ss -tinH state established "( dport = :2049 )" 2>/dev/null
  sleep 1
done > /tmp/flow-client-$1.log 2>&1' >/dev/null 2>&1 &
echo started
EOS
}
# DS side: the pod netns via nsenter with the HOST's ss binary — the pod
# image may not carry iproute2, and the host binary in the pod netns is
# the same measurement.
start_ds_sampler() {  # tag
  "$HERE/nodesh.sh" "$DSNODE" - <<EOS >/dev/null 2>&1
touch /tmp/flowsample.on
nohup sh -c 'while [ -e /tmp/flowsample.on ]; do
  echo "T \$(date +%s.%N)"
  nsenter -t $DS_PID -n ss -tinH state established 2>/dev/null
  sleep 1
done > /tmp/flow-ds-$1.log 2>&1' >/dev/null 2>&1 &
echo started
EOS
}
stop_samplers() {
  "$HERE/nodesh.sh" "$CLIENT" 'rm -f /tmp/flowsample.on' >/dev/null 2>&1
  "$HERE/nodesh.sh" "$DSNODE" 'rm -f /tmp/flowsample.on' >/dev/null 2>&1
  sleep 1.5
}
collect() {  # tag
  "$HERE/nodesh.sh" "$CLIENT" "cat /tmp/flow-client-$1.log 2>/dev/null" \
    > "$OUT/flow-client-$1.log" 2>/dev/null
  "$HERE/nodesh.sh" "$DSNODE" "cat /tmp/flow-ds-$1.log 2>/dev/null" \
    > "$OUT/flow-ds-$1.log" 2>/dev/null
}

# Client per-cpu softirq accounting (jiffies from /proc/stat col 8 +
# NET_RX from /proc/softirqs) — snapshot form, deltas in analysis.
client_softirq() {
  "$HERE/nodesh.sh" "$CLIENT" '
    awk "/^cpu[0-9]/{print \"STAT\", \$1, \$8}" /proc/stat
    awk "/NET_RX/{print \"NETRX\", \$0}" /proc/softirqs
    echo "TICK $(getconf CLK_TCK 2>/dev/null || echo 100)"' 2>/dev/null
}

one_read() {  # n
  local n=$1 t0 t1 res mibps
  drop_client_cache
  start_client_sampler "$n"; start_ds_sampler "$n"
  client_softirq > "$OUT/softirq-$n-pre.txt"
  t0=$(date +%s%N)
  res=$(kubectl exec "$POD" -- python3 /bench/bench.py read --dir /data \
        --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream \
        --workers "$WORKERS" 2>&1 | grep RESULT)
  t1=$(date +%s%N)
  client_softirq > "$OUT/softirq-$n-post.txt"
  stop_samplers
  collect "$n"
  mibps=$(sed -n 's/.*mibps=\([0-9]*\).*/\1/p' <<<"$res")
  [ -n "${mibps:-}" ] || die "read $n produced no RESULT"
  echo "$n $mibps $t0 $t1" >> "$OUT/reads.txt"
  say "  read #$n: $mibps MiB/s (samples: client $(grep -c '^T ' "$OUT/flow-client-$n.log" 2>/dev/null || echo 0), ds $(grep -c '^T ' "$OUT/flow-ds-$n.log" 2>/dev/null || echo 0))"
}

# ── instrument self-test: every meter must MOVE ─────────────────────────
say "── instrument self-test (one throwaway read) ──"
: > "$OUT/reads.txt"
one_read 0
python3 - "$OUT" <<'PY' || exit 1
import re, sys, glob
out = sys.argv[1]
cl = open(f"{out}/flow-client-0.log").read()
ds = open(f"{out}/flow-ds-0.log").read()
n_cl = cl.count("\nT ") + cl.startswith("T ")
if len(cl.splitlines()) < 6:
    print("✗ SELF-TEST: client sampler produced almost nothing"); sys.exit(1)
recv = [int(m) for m in re.findall(r"bytes_received:(\d+)", cl)]
if not recv or max(recv) < 1 << 30:
    print(f"✗ SELF-TEST: client bytes_received never exceeded 1 GiB "
          f"(max {max(recv) if recv else 0}) — wrong filter, wrong netns, or no flows")
    sys.exit(1)
acked = [int(m) for m in re.findall(r"bytes_acked:(\d+)", ds)]
if not acked or max(acked) < 1 << 30:
    print(f"✗ SELF-TEST: DS bytes_acked never exceeded 1 GiB "
          f"(max {max(acked) if acked else 0}) — nsenter/netns wrong")
    sys.exit(1)
print(f"✓ self-test: client sees NFS flows (max recv {max(recv)>>20} MiB), "
      f"DS sees its side (max acked {max(acked)>>20} MiB)")
PY
say "  ✓ all flow meters move — proceeding"

# ── phase 1: on-wire shape (tcpdump, 20 s, during a background read) ────
say "── phase 1: on-wire packet shape ──"
drop_client_cache
kubectl exec "$POD" -- python3 /bench/bench.py read --dir /data \
  --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream --workers "$WORKERS" \
  >/dev/null 2>&1 &
BGREAD=$!
sleep 2
"$HERE/nodesh.sh" "$CLIENT" \
  "timeout 12 tcpdump -ni any -c 4000 2>/dev/null | awk '
     /8472|4789|6081/{encap++}
     /51820|51871/{wg++}
     /2049/{plain++}
     END{print \"WIRE client encap=\" encap+0, \"wg=\" wg+0, \"nfs_plain=\" plain+0}'" \
  2>/dev/null | tail -1 | tee "$OUT/wire-client.txt"
"$HERE/nodesh.sh" "$DSNODE" \
  "timeout 12 tcpdump -ni any -c 4000 2>/dev/null | awk '
     /8472|4789|6081/{encap++}
     /51820|51871/{wg++}
     /2049/{plain++}
     END{print \"WIRE ds encap=\" encap+0, \"wg=\" wg+0, \"nfs_plain=\" plain+0}'" \
  2>/dev/null | tail -1 | tee "$OUT/wire-ds.txt"
wait $BGREAD 2>/dev/null || true

# ── phase 2: the measured reads ─────────────────────────────────────────
say "── phase 2: $N_READS sampled reads ──"
for i in $(seq 1 "$N_READS"); do one_read "$i"; done

# ── phase 3: single-flow iperf3, both directions, host + pod net ────────
say "── phase 3: iperf3 single-flow baselines ──"
"$HERE/nodesh.sh" "$DSNODE" 'command -v iperf3 >/dev/null || dnf install -y -q iperf3 2>/dev/null; command -v iperf3 && echo IPERF_OK' 2>/dev/null | tail -1
"$HERE/nodesh.sh" "$CLIENT" 'command -v iperf3 >/dev/null || dnf install -y -q iperf3 2>/dev/null; command -v iperf3 && echo IPERF_OK' 2>/dev/null | tail -1
"$HERE/nodesh.sh" "$DSNODE" 'pkill -f iperf3 2>/dev/null; nohup iperf3 -s -p 5301 >/dev/null 2>&1 & echo srv' >/dev/null 2>&1
sleep 2
for dir in fwd rev; do
  FLAG=""; [ "$dir" = "rev" ] && FLAG="-R"
  "$HERE/nodesh.sh" "$CLIENT" \
    "iperf3 -c $DSNODE_IP -p 5301 -t 6 -P 1 $FLAG -J 2>/dev/null" 2>/dev/null \
    > "$OUT/iperf3-host-$dir.json" || true
  BPS=$(python3 -c "
import json,sys
try:
    d=json.load(open('$OUT/iperf3-host-$dir.json'))
    print(f\"{d['end']['sum_received']['bits_per_second']/8/2**20:.0f}\")
except Exception: print('?')" 2>/dev/null)
  say "  host-net single-flow $dir (client<->$DSNODE_IP): ${BPS} MiB/s"
done
"$HERE/nodesh.sh" "$DSNODE" 'pkill -f iperf3 2>/dev/null' >/dev/null 2>&1

say "── phase 3b: pod-net single-flow ──"
kubectl delete pod ipf-s ipf-c --ignore-not-found --wait=true >/dev/null 2>&1
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: ipf-s}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $DSNODE}
  containers: [{name: s, image: networkstatic/iperf3, args: ["-s","-p","5301"]}]
---
apiVersion: v1
kind: Pod
metadata: {name: ipf-c}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  containers: [{name: c, image: networkstatic/iperf3, command: ["sleep","600"]}]
YAML
if kubectl wait --for=condition=Ready pod/ipf-s pod/ipf-c --timeout=120s >/dev/null 2>&1; then
  IPS=$(kubectl get pod ipf-s -o jsonpath='{.status.podIP}')
  for dir in fwd rev; do
    FLAG=""; [ "$dir" = "rev" ] && FLAG="-R"
    kubectl exec ipf-c -- iperf3 -c "$IPS" -p 5301 -t 6 -P 1 $FLAG -J \
      > "$OUT/iperf3-pod-$dir.json" 2>/dev/null || true
    BPS=$(python3 -c "
import json,sys
try:
    d=json.load(open('$OUT/iperf3-pod-$dir.json'))
    print(f\"{d['end']['sum_received']['bits_per_second']/8/2**20:.0f}\")
except Exception: print('?')" 2>/dev/null)
    say "  pod-net single-flow $dir: ${BPS} MiB/s"
  done
else
  say "  (pod-net iperf3 pods not Ready — skipping, host-net numbers stand)"
fi
kubectl delete pod ipf-s ipf-c --ignore-not-found --wait=false >/dev/null 2>&1

# ── phase 4: analysis ───────────────────────────────────────────────────
say "── analysis ──"
python3 "$HERE/pnfs-flow-analysis.py" "$OUT" || die "analysis failed"

kubectl delete pod "$POD" --ignore-not-found --wait=false >/dev/null 2>&1
kubectl delete configmap cm-flowclass --ignore-not-found >/dev/null 2>&1
say "raw data: $OUT"
