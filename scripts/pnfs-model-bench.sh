#!/usr/bin/env bash
# Run the model-shaped benchmark on a client node against a pNFS PVC, with
# the page cache dropped and the per-DS egress measured.
#
#   ./scripts/pnfs-model-bench.sh <client-node> <pvc> <label> [shards] [gib]
#
# Two things this does that a bare fio run does not, and both change the
# answer:
#
#  1. DROPS THE PAGE CACHE on the client between write and read. The model
#     bench is deliberately buffered (that is what a checkpoint load is), so
#     a 32 GiB read on a 96 GiB client would otherwise be served entirely
#     from RAM and report a number with no storage in it at all.
#  2. Reports how many data servers actually served, from their own NIC
#     counters. Striping that silently collapses to one server is the exact
#     failure this whole line of work exists to catch, and the client cannot
#     see it.
set -uo pipefail
CLIENT=${1:?usage: pnfs-model-bench.sh <client-node> <pvc> <label> [shards] [gib]}
PVC=${2:?}
LABEL=${3:?}
SHARDS=${4:-8}
GIB=${5:-4}
MODE=${MODE:-mmap}
WORKERS=${WORKERS:-1}

NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
: "${KUBECONFIG:?set KUBECONFIG}"
POD="modelbench-$(echo "$LABEL" | tr -cd 'a-z0-9')"

DS_NODES=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u)

tx_snapshot() {
  for n in $DS_NODES; do
    printf "%s %s\n" "$n" "$("$HERE/nodesh.sh" "$n" \
      'IF=$(awk "\$2==\"00000000\"{print \$1; exit}" /proc/net/route)
       awk -v i="$IF" "\$1 ~ \"^\"i\":\" { gsub(/:/,\" \"); print \$10 }" /proc/net/dev' \
      2>/dev/null | head -1)"
  done
}

kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
kubectl create configmap "mb-$POD" --from-file=bench.py="$HERE/pnfs-model-bench.py" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $POD}
spec:
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  containers:
  - name: b
    image: python:3.12-alpine
    command: ["sh","-c","sleep 100000"]
    volumeMounts:
    - {name: d, mountPath: /data}
    - {name: s, mountPath: /bench}
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: $PVC}
  - name: s
    configMap: {name: mb-$POD}
YAML
kubectl wait --for=condition=Ready "pod/$POD" --timeout=300s >/dev/null 2>&1 \
  || { echo "✗ $POD not Ready"; kubectl describe pod "$POD" | tail -20; exit 1; }

# SKIP_WRITE reuses shards a previous run laid down. Only safe when the
# geometry is unchanged — placement is pinned per file at first LAYOUTGET,
# so shards written under a different stripe config keep the OLD one and
# would be measured under the new label. Use it for readahead sweeps (a
# client-side knob) and never across a StorageClass change.
if [ -n "${SKIP_WRITE:-}" ]; then
  echo "▶ $LABEL — reusing the existing checkpoint (SKIP_WRITE)"
  kubectl exec "$POD" -- sh -c 'ls /data/model-*.safetensors 2>/dev/null | wc -l' \
    | sed 's/^/  shards present: /'
else
  echo "▶ $LABEL — writing $SHARDS x ${GIB} GiB checkpoint on $CLIENT"
  kubectl exec "$POD" -- sh -c \
    "rm -f /data/model-*.safetensors; python3 /bench/bench.py write --dir /data \
     --shards $SHARDS --shard-gib $GIB --workers $WORKERS" 2>&1 | grep -E "RESULT|ERROR"
fi

# THE STEP THAT MAKES THE READ REAL.
echo "▶ dropping the client page cache"
"$HERE/nodesh.sh" "$CLIENT" 'sync; echo 3 > /proc/sys/vm/drop_caches; \
   awk "/^MemAvailable/{print \"  MemAvailable \" \$2/1048576 \" GiB\"}" /proc/meminfo' \
   2>/dev/null | grep MemAvailable

# READAHEAD IS THE KNOB THIS BENCHMARK EXISTS TO EXERCISE. A sequential
# reader's parallelism across a striped file is bounded by how far ahead the
# kernel reads: with a 15 MiB default window and an 8 MiB stripe unit, only
# about two of five data servers can have work in flight at once, however
# wide the stripe. It is writable at runtime, so the hypothesis is testable
# on a live mount with no image, no remount and no re-provision.
if [ -n "${RA_KB:-}" ]; then
  echo "▶ setting NFS readahead to ${RA_KB} KiB"
  "$HERE/nodesh.sh" "$CLIENT" "
    for d in \$(awk '{for(i=1;i<=NF;i++) if(\$i==\"-\"){print \$3, \$(i+1); break}}' \
               /proc/1/mountinfo | awk '\$2 ~ /^nfs/ {print \$1}' | sort -u); do
      [ -w /sys/class/bdi/\$d/read_ahead_kb ] || continue
      echo $RA_KB > /sys/class/bdi/\$d/read_ahead_kb
      echo \"  bdi \$d read_ahead_kb=\$(cat /sys/class/bdi/\$d/read_ahead_kb)\"
    done" 2>/dev/null | grep read_ahead_kb
fi

echo "▶ reading it back (mode=$MODE workers=$WORKERS)"
BEFORE=$(tx_snapshot)
T0=$(date +%s)
kubectl exec "$POD" -- sh -c \
  "python3 /bench/bench.py read --dir /data --shards $SHARDS --shard-gib $GIB \
   --mode $MODE --workers $WORKERS" 2>&1 | grep -E "RESULT|WARNING|ERROR"
T1=$(date +%s)
AFTER=$(tx_snapshot)

echo "  per-DS egress during the load:"
python3 - "$BEFORE" "$AFTER" "$((T1 - T0))" <<'PY'
import sys
a = dict(l.split() for l in sys.argv[1].strip().splitlines() if len(l.split()) == 2)
b = dict(l.split() for l in sys.argv[2].strip().splitlines() if len(l.split()) == 2)
dur = max(1.0, float(sys.argv[3]))
rows = [(k, int(b[k]) - int(a[k])) for k in sorted(a) if k in b]
tot = sum(d for _, d in rows) or 1
for k, d in rows:
    print(f"    {k:<16}{d/1048576/dur:8.1f} MiB/s  {100.0*d/tot:5.1f}%")
served = [d for _, d in rows if d > 0.02 * tot]
print(f"    -> {len(served)} of {len(rows)} data servers actually served")
PY
kubectl delete pod "$POD" --ignore-not-found --wait=false >/dev/null 2>&1
kubectl delete configmap "mb-$POD" --ignore-not-found >/dev/null 2>&1
