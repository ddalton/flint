#!/usr/bin/env bash
# run-fio.sh -- run the fio matrix (matrix.tsv) against one StorageClass and
# record, per test and repetition, fio's JSON plus the storage driver's CPU
# on every node during the measured window. Driver-agnostic: the driver is
# named by its StorageClass and by the process patterns the samplers count.
#
# Plan: docs/plans/flint-csi-benchmark-plan.md (§2 Q1-Q3, §3 Q9, §5).
#
#   SC=flint-r3 DRIVER=flint CPU_PATTERNS='spdk=spdk_tgt,agent=csi-driver' \
#     OUT=results/flint-r3 ./run-fio.sh
#
# Env:
#   SC            StorageClass under test (required)
#   DRIVER        label for the results (required)
#   CPU_PATTERNS  label=extended-regex,... matched against each process's
#                 cmdline on every node (required)
#   OUT           results directory (required; must not exist)
#   SIZE          PVC size (100Gi)          FILE_SIZE  fio file (90G)
#   RUNTIME       measured seconds (120)    RAMP       unmeasured lead-in (30)
#   REPS          repetitions of the whole matrix (3)
#   TESTS         space-separated subset of matrix names (all)
#   FIO_NODE      pin the fio pod to this node (unset: scheduler's choice)
#   FIO_IMAGE     image with fio, or alpine to `apk add fio` (alpine:3.20)
#   KEEP          1 = leave the PVC and fio pod in place (0)
#   PRECONDITION  0 = skip the full write (only for experiments that reuse a
#                 KEEP=1 volume on purpose); default 1
#   REACTOR_TICKS optional host command printing "<busy> <idle> <reactors>"
#                 cumulative ticks of the driver's polling reactors (env.sh);
#                 sampled at both edges of each window, because a polling
#                 core shows 100% process CPU whether it works or not
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
: "${SC:?set SC}" "${DRIVER:?set DRIVER}" "${CPU_PATTERNS:?set CPU_PATTERNS}" "${OUT:?set OUT}"
SIZE="${SIZE:-100Gi}" FILE_SIZE="${FILE_SIZE:-90G}"
RUNTIME="${RUNTIME:-120}" RAMP="${RAMP:-30}" REPS="${REPS:-3}"
TESTS="${TESTS:-}" FIO_NODE="${FIO_NODE:-}" FIO_IMAGE="${FIO_IMAGE:-alpine:3.20}" KEEP="${KEEP:-0}"
NS=flint-bench
PVC="bench-${DRIVER}"
POD="fio-${DRIVER}"

step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }
[ "$RUNTIME" -ge 5 ] || fail "RUNTIME must be >= 5 s (the sampler window is RUNTIME-2)"
[ ! -e "$OUT" ] || fail "$OUT exists; results are never overwritten"
kubectl get storageclass "$SC" >/dev/null || fail "no StorageClass $SC"
mkdir -p "$OUT"

cleanup() {
  rc=$?
  if [ "$KEEP" != 1 ]; then
    kubectl -n "$NS" delete pod "$POD" --wait=true --timeout=180s >/dev/null 2>&1 || true
    kubectl -n "$NS" delete pvc "$PVC" --wait=true --timeout=300s >/dev/null 2>&1 || true
  fi
  [ "$rc" = 0 ] || echo "FAILED rc=$rc; partial results in $OUT" >&2
  exit "$rc"
}
trap cleanup EXIT

step "samplers on every node"
kubectl apply -f "$HERE/sampler.yaml" >/dev/null
kubectl -n "$NS" rollout status ds/bench-sampler --timeout=300s >/dev/null

step "PVC $PVC ($SIZE on $SC) and fio pod $POD"
pin=""
[ -z "$FIO_NODE" ] || pin="nodeName: $FIO_NODE"
install=""
case "$FIO_IMAGE" in alpine:*) install="apk add --no-cache fio >/dev/null &&" ;; esac
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: $PVC, namespace: $NS}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: $SC
  resources: {requests: {storage: $SIZE}}
---
apiVersion: v1
kind: Pod
metadata: {name: $POD, namespace: $NS, labels: {app: bench-fio}}
spec:
  $pin
  restartPolicy: Never
  containers:
  - name: fio
    image: $FIO_IMAGE
    command: ["sh", "-c", "$install touch /tmp/ready && sleep infinity"]
    readinessProbe:
      exec: {command: ["test", "-f", "/tmp/ready"]}
      periodSeconds: 2
    volumeMounts: [{name: data, mountPath: /data}]
  volumes:
  - name: data
    persistentVolumeClaim: {claimName: $PVC}
EOF
kubectl -n "$NS" wait --for=condition=Ready "pod/$POD" --timeout=600s >/dev/null

step "environment"
{
  echo "date_utc: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "driver: $DRIVER"
  echo "storageclass: $SC"
  echo "pvc_size: $SIZE"
  echo "file_size: $FILE_SIZE"
  echo "runtime_s: $RUNTIME"
  echo "ramp_s: $RAMP"
  echo "reps: $REPS"
  echo "precondition: ${PRECONDITION:-1}"
  echo "cpu_patterns: $CPU_PATTERNS"
  echo "reactor_ticks: ${REACTOR_TICKS:+yes}"
  echo "fio_node: $(kubectl -n "$NS" get pod "$POD" -o jsonpath='{.spec.nodeName}')"
  echo "fio_version: $(kubectl -n "$NS" exec "$POD" -- fio --version)"
  echo "bench_git: $(git -C "$HERE" rev-parse --short HEAD 2>/dev/null || echo unknown)"
} > "$OUT/env.yaml"
kubectl get storageclass "$SC" -o yaml > "$OUT/storageclass.yaml"
kubectl get nodes -o wide > "$OUT/nodes.txt"
kubectl version -o yaml > "$OUT/kubectl-version.yaml" 2>/dev/null || true
kubectl get pods -A -o jsonpath='{range .items[*]}{.metadata.namespace}{"\t"}{.metadata.name}{"\t"}{range .spec.containers[*]}{.image}{" "}{end}{"\n"}{end}' > "$OUT/images.tsv"
kubectl -n "$NS" get pvc "$PVC" -o yaml > "$OUT/pvc.yaml"
kubectl get pv "$(kubectl -n "$NS" get pvc "$PVC" -o jsonpath='{.spec.volumeName}')" -o yaml > "$OUT/pv.yaml"
cp "$HERE/matrix.tsv" "$OUT/"
cat "$OUT/env.yaml" >&2

samplers=$(kubectl -n "$NS" get pods -l app=bench-sampler -o jsonpath='{.items[*].metadata.name}')

# ENA "allowance exceeded" counters (AWS throttling: bandwidth, pps, conntrack)
# read in the HOST network namespace through the hostPID samplers. A rise
# between before and after means the instance's network limits, not the
# storage, bound part of the run. Off AWS (no ENA) this records "n/a".
net_counters() {  # <label>
  local s
  for s in $samplers; do
    kubectl -n "$NS" exec "$s" -- sh -c '
      i=$(nsenter -t 1 -n -m -- ip route get 1.1.1.1 2>/dev/null | sed -n "s/.* dev \([^ ]*\).*/\1/p")
      [ -n "$i" ] && nsenter -t 1 -n -m -- ethtool -S "$i" 2>/dev/null | grep -E "allowance_exceeded|bw_in|bw_out" || echo n/a' \
      > "$OUT/net-$1-$s.txt" 2>&1 || true
  done
}
net_counters before

fio_common="--filename=/data/bench.dat --size=$FILE_SIZE --direct=1 --ioengine=libaio \
--group_reporting --output-format=json --randrepeat=0 --random_generator=tausworthe64"

if [ "${PRECONDITION:-1}" = 1 ]; then
  step "precondition: write all of $FILE_SIZE once"
  kubectl -n "$NS" exec "$POD" -- fio --name=precondition $fio_common \
    --rw=write --bs=1m --iodepth=16 --numjobs=1 > "$OUT/precondition.json"
else
  step "precondition SKIPPED (PRECONDITION=0)"
fi

run_one() {  # <dir> <name> <rw> <bs> <iodepth> <numjobs> <rwmixread>
  local dir=$1 name=$2 rw=$3 bs=$4 qd=$5 nj=$6 mix=$7 s pids=()
  mkdir -p "$dir"
  for s in $samplers; do
    kubectl -n "$NS" exec "$s" -- /bench/sample.sh "$(( RAMP + 1 ))" "$(( RUNTIME - 2 ))" "$CPU_PATTERNS" \
      > "$dir/cpu-$s.json" 2> "$dir/cpu-$s.err" &
    pids+=($!)
  done
  if [ -n "${REACTOR_TICKS:-}" ]; then
    for s in $samplers; do
      ( tick() { kubectl -n "$NS" exec "$s" -- nsenter -t 1 -m -u -n -i -p -- sh -c "$REACTOR_TICKS"; }
        sleep "$(( RAMP + 1 ))"; a=$(tick); sleep "$(( RUNTIME - 2 ))"; b=$(tick)
        echo "$a $b" ) > "$dir/reactor-$s.txt" 2> "$dir/reactor-$s.err" &
      pids+=($!)
    done
  fi
  local extra=""
  [ "$mix" = - ] || extra="--rwmixread=$mix"
  kubectl -n "$NS" exec "$POD" -- fio --name="$name" $fio_common \
    --rw="$rw" --bs="$bs" --iodepth="$qd" --numjobs="$nj" $extra \
    --time_based --runtime="$RUNTIME" --ramp_time="$RAMP" \
    --percentile_list=50:99:99.9 > "$dir/fio.json"
  for p in "${pids[@]}"; do wait "$p" || fail "a sampler failed in $dir (see cpu-*.err, reactor-*.err)"; done
}

step "idle: the driver's CPU with no I/O (a polling engine's floor)"
mkdir -p "$OUT/idle"
for s in $samplers; do
  kubectl -n "$NS" exec "$s" -- /bench/sample.sh 0 "$(( RUNTIME - 2 ))" "$CPU_PATTERNS" \
    > "$OUT/idle/cpu-$s.json" 2> "$OUT/idle/cpu-$s.err" &
done
wait

for rep in $(seq 1 "$REPS"); do
  while read -r name rw bs qd nj mix; do
    case "$name" in ''|'#'*) continue ;; esac
    if [ -n "$TESTS" ]; then case " $TESTS " in *" $name "*) ;; *) continue ;; esac; fi
    step "rep $rep/$REPS: $name ($rw bs=$bs qd=$qd jobs=$nj)"
    run_one "$OUT/$name/rep$rep" "$name" "$rw" "$bs" "$qd" "$nj" "$mix"
  done < "$HERE/matrix.tsv"
done

net_counters after
step "done: $OUT"
