#!/usr/bin/env bash
# DS SCALING, MEASURED SO THE ANSWER SURVIVES ITS OWN CONTROLS.
#
#   ./scripts/pnfs-ds-scaling-sweep.sh <client-node> [max-width] [storage-class]
#
# WHY THIS EXISTS. Three DS-scaling numbers were published off one cluster
# (5.33x, then 3.0x, then a width curve) and controls killed all three. The
# cause was never flint: the rig drifted ~2x within a session, so any ratio
# assembled from runs taken minutes apart was arithmetic on two different
# machines. The width curve had a second problem on top — measured
# ascending it said one thing, descending it said something up to 1.6x
# different, which is the sweep telling you it is not measuring a property
# of the width at all.
#
# So this script enforces, mechanically, the four rules that would have
# caught every one of those:
#
#   1. ONE WINDOW. Every width is measured back-to-back in a single run.
#      Nothing here compares against a number from an earlier session.
#   2. BOTH DIRECTIONS. Widths run 1..N then N..1. If a width's result
#      depends on when it ran, the two passes disagree and the report says
#      so instead of averaging the disagreement away.
#   3. ONE MOUNT PER CLIENT. Every pNFS PVC on a node shares one
#      `nfs_client` and its nconnect pool, so five warm pods on one client
#      is five volumes sharing one transport. Pods are created and DELETED
#      around each point, at a cost of ~30s per measurement, deliberately.
#   4. THE BASELINE IS RE-MEASURED AT THE END. The first width is run
#      again last. If it does not come back, the window did not hold and
#      the whole sweep is void — stated as a verdict, not left for a
#      reader to notice.
#
# It also samples the ENA allowance counters at every point. "Up to 25
# Gbps" is a BURST over a much lower guaranteed baseline (i3en.xlarge:
# 4.2 Gbps = 501 MiB/s; i3en.3xlarge: 12.5 Gbps = 1490 MiB/s), and a
# session that exhausts the credit halves quietly. Any point whose NIC
# counters moved is reported as METERED — that number is AWS, not flint.
set -uo pipefail
CLIENT=${1:?usage: pnfs-ds-scaling-sweep.sh <client-node> [max-width] [storage-class]}
MAXW=${2:-5}
SC=${3:-flint-pnfs}

NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${SWEEP_OUT:-/tmp/pnfs-ds-sweep}
SHARDS=${SHARDS:-4}
SHARD_GIB=${SHARD_GIB:-2}
WORKERS=${WORKERS:-4}
MODE=${MODE:-stream}
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

DS_NODES=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u)
DS_COUNT=$(echo "$DS_NODES" | grep -c .)
[ "$DS_COUNT" -ge "$MAXW" ] || { echo "only $DS_COUNT DSes for max width $MAXW"; exit 1; }

echo "▶ DS scaling sweep: widths 1..$MAXW on $CLIENT, ${SHARDS}x${SHARD_GIB}GiB $MODE/$WORKERS"
echo "  data servers: $(echo "$DS_NODES" | tr '\n' ' ')"

# ── NIC allowance: the instrument that says "this number is AWS" ────────
# Summed over every DS and the client. Sampled either side of each read,
# so a metered point is attributable to the point, not to the session.
allowance_sum() {
  local total=0 v
  for n in $DS_NODES $CLIENT; do
    v=$("$HERE/nodesh.sh" "$n" \
      'IF=$(awk "\$2==\"00000000\"{print \$1; exit}" /proc/net/route)
       ethtool -S $IF 2>/dev/null | awk "/allowance_exceeded/{s+=\$2} END{print s+0}"' \
      2>/dev/null | tail -1)
    total=$((total + ${v:-0}))
  done
  echo "$total"
}

# ── one PVC per width ───────────────────────────────────────────────────
for w in $(seq 1 "$MAXW"); do
  kubectl get pvc "dsw$w" >/dev/null 2>&1 && continue
  cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: dsw$w}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: $SC
  resources: {requests: {storage: 64Gi}}
YAML
done

# A pod bound to exactly one width's PVC. Created per measurement and torn
# down after it, so the client never holds two pNFS mounts at once.
bench_pod() {  # width, action(write|read) -> prints RESULT line
  local w=$1 action=$2 p="dssweep-w$w"
  kubectl delete pod "$p" --ignore-not-found --wait=true >/dev/null 2>&1
  kubectl create configmap "cm-dssweep" \
    --from-file=bench.py="$HERE/pnfs-model-bench.py" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $p}
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
    persistentVolumeClaim: {claimName: dsw$w}
  - name: s
    configMap: {name: cm-dssweep}
YAML
  kubectl wait --for=condition=Ready "pod/$p" --timeout=300s >/dev/null 2>&1 || {
    echo "RESULT mibps=0 seconds=0 ERR=pod-not-ready"; return; }
  # Drop the CLIENT's page cache, never the server's: this measures the
  # path to the data servers, and a warm client cache measures memcpy.
  "$HERE/nodesh.sh" "$CLIENT" 'sync; echo 3 > /proc/sys/vm/drop_caches' >/dev/null 2>&1
  kubectl exec "$p" -- python3 /bench/bench.py "$action" --dir /data \
    --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode "$MODE" \
    --workers "$WORKERS" 2>&1 | grep RESULT
  kubectl delete pod "$p" --ignore-not-found --wait=true >/dev/null 2>&1
}

mibps_of() { sed -n 's/.*mibps=\([0-9]*\).*/\1/p' <<<"$1"; }

# ── layout: write each width's checkpoint once, before any measurement ──
# Placement is pinned per file at first LAYOUTGET, so the write IS the
# layout run. Reads afterwards measure only reads — a read job that lays
# out its own files inside the measured window is how a previous round
# manufactured a 3.4x "regression".
echo
echo "── laying out checkpoints (once per width) ──"
for w in $(seq 1 "$MAXW"); do
  r=$(bench_pod "$w" write)
  printf "  width %-2s write %6s MiB/s\n" "$w" "$(mibps_of "$r")"
done

# ── the sweep ───────────────────────────────────────────────────────────
: > "$OUT/points.txt"
measure() {  # width, pass-label
  local w=$1 lab=$2 a0 a1 r m
  a0=$(allowance_sum)
  r=$(bench_pod "$w" read)
  a1=$(allowance_sum)
  m=$(mibps_of "$r")
  printf "%s %s %s %s\n" "$lab" "$w" "${m:-0}" "$((a1 - a0))" >>"$OUT/points.txt"
  printf "  %-4s width %-2s %6s MiB/s%s\n" "$lab" "$w" "${m:-0}" \
    "$([ "$((a1 - a0))" -gt 0 ] && echo "   ** METERED: NIC allowance moved by $((a1 - a0)) — this is AWS, not flint **")"
}

echo
echo "── ascending ──"
for w in $(seq 1 "$MAXW"); do measure "$w" up; done
echo
echo "── descending ──"
for w in $(seq "$MAXW" -1 1); do measure "$w" down; done
echo
echo "── baseline re-measured (did the window hold?) ──"
measure 1 end

# ── verdict ─────────────────────────────────────────────────────────────
echo
python3 - "$OUT/points.txt" <<'PY'
import sys, collections
pts = collections.defaultdict(dict)
metered = 0
for line in open(sys.argv[1]):
    lab, w, m, a = line.split()
    pts[int(w)][lab] = int(m)
    metered += int(a)

first = pts[1].get('up', 0)
last  = pts[1].get('end', 0)
print(f"{'width':>6}{'up':>9}{'down':>9}{'spread':>9}")
for w in sorted(pts):
    u, d = pts[w].get('up', 0), pts[w].get('down', 0)
    sp = f"{max(u,d)/min(u,d):.2f}x" if u and d else "—"
    print(f"{w:>6}{u:>9}{d:>9}{sp:>9}")

print()
# The window check comes FIRST: if the rig moved, no ratio below it means
# anything, and saying so is the whole point of re-running the baseline.
if first and last:
    drift = max(first, last) / min(first, last)
    print(f"baseline width-1: {first} at the start, {last} at the end — {drift:.2f}x drift")
    if drift > 1.15:
        print("*** THE WINDOW DID NOT HOLD. Every ratio in this sweep is VOID. ***")
        print("    Re-run on a quiet fleet; do not report a scaling number from this data.")
        sys.exit(0)
else:
    print("*** baseline missing — cannot certify the window; treat as VOID ***")
    sys.exit(0)

bad = [w for w in pts if pts[w].get('up') and pts[w].get('down')
       and max(pts[w]['up'], pts[w]['down']) / min(pts[w]['up'], pts[w]['down']) > 1.25]
if bad:
    print(f"*** widths {bad} disagree by >1.25x between passes — order-dependent,")
    print("    so the curve is not a property of the width. VOID for those points. ***")
    sys.exit(0)

if metered:
    print(f"*** NIC allowance counters moved by {metered} during the sweep. At least one")
    print("    point was rate-limited by AWS, not by flint. Size up the nodes or")
    print("    re-measure at steady state; do not publish this as a storage result. ***")
    sys.exit(0)

base = min((pts[w].get('up', 0) + pts[w].get('down', 0)) / 2 for w in (1,) if w in pts)
if base:
    print("window held, passes agree, no metering — scaling vs width 1:")
    for w in sorted(pts):
        avg = (pts[w].get('up', 0) + pts[w].get('down', 0)) / 2
        print(f"  width {w}: {avg:7.0f} MiB/s   {avg/base:5.2f}x")
PY

echo
echo "  raw: $OUT/points.txt"
kubectl delete configmap cm-dssweep --ignore-not-found >/dev/null 2>&1
