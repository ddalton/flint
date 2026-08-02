#!/usr/bin/env bash
# Instrumented pNFS read/write against one client node, with the DS fan-out
# and the client's NFS RPC accounting measured across the SAME window.
#
#   ./scripts/pnfs-fanout-diag.sh <client-node> <pvc> <label> [rw] [seconds]
#
# WHY. runax measured pNFS reads getting 2.7x WORSE as the client node grew
# (1167 MiB/s at 4 vCPU, 423 at 12) with writes flat, and five hypotheses
# died without naming a cause. Every one of those tests measured the client
# only. The two things nobody had looked at are (a) whether the big client
# still spreads its reads over all N data servers and (b) what the client's
# own RPC layer says about where the time goes.
#
# 423 MiB/s is suspiciously near the 396 MiB/s a SINGLE data server was
# measured to serve over pNFS. If the 12-vCPU client is quietly reading from
# one DS, the "regression" is a fan-out bug and has nothing to do with cores.
# This script is built to make that visible or rule it out.
#
# Output: per-DS tx bytes over the fio window with each DS's share, the
# client's mountstats delta (ops, bytes, RTT, backlog wait), client CPU
# breakdown, and fio's own throughput. Raw samples are kept so a suspicious
# aggregate can be re-read as a time series.
set -uo pipefail
CLIENT=${1:?usage: pnfs-fanout-diag.sh <client-node> <pvc> <label> [read|write] [seconds]}
PVC=${2:?}
LABEL=${3:?}
RW=${4:-read}
DUR=${5:-60}

NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${DIAG_OUT:-/tmp/pnfs-diag}/$LABEL
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

# Sampler runs a bit longer than fio on both sides so the fio window is
# strictly inside it — deltas are then computed by timestamp, not by hoping
# the pods started together.
SAMPLE=$((DUR + 40))

echo "▶ $LABEL: $RW on $CLIENT via $PVC, ${DUR}s"

DS_NODES=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' | sort -u)
[ -n "$DS_NODES" ] || { echo "no DS pods found"; exit 1; }
echo "  data servers: $(echo "$DS_NODES" | tr '\n' ' ')"

# ── per-second NIC counters on every DS and on the client ───────────────
# Interface is auto-detected from the default route: ENA names vary
# (ens5/eth0) and hardcoding one silently yields an empty column.
sampler() {
  cat <<SH
IF=\$(awk '\$2=="00000000"{print \$1; exit}' /proc/net/route)
for i in \$(seq 1 $SAMPLE); do
  awk -v t="\$(date +%s)" -v i="\$IF" '\$1 ~ "^"i":" { gsub(/:/," "); print t, \$2, \$10 }' /proc/net/dev
  sleep 1
done
SH
}
for n in $DS_NODES $CLIENT; do
  sampler | "$HERE/nodesh.sh" "$n" - >"$OUT/nic-$n.txt" 2>/dev/null &
done

# ── client-side snapshots that bracket the run ──────────────────────────
snap() {
  "$HERE/nodesh.sh" "$CLIENT" "date +%s; echo ---STAT---; cat /proc/stat; \
     echo ---SOFTIRQS---; cat /proc/softirqs; \
     echo ---INTERRUPTS---; cat /proc/interrupts; \
     echo ---MOUNTSTATS---; cat /proc/1/mountstats" >"$OUT/client-$1.txt" 2>/dev/null
}
snap before

# ── the load ────────────────────────────────────────────────────────────
POD="fiodiag-$(echo "$LABEL" | tr -cd 'a-z0-9')"
kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
# RUN write BEFORE read for any given client+pvc, and note THE JOB NAME IS
# FIXED AT `bench`. fio derives its filenames from --name, so a read job
# named "read" does NOT reuse what a write job named "write" laid down — it
# creates `read.N.0` itself, writing 16 GiB inside the measured window.
# That cost a full round of measurements here: the first 12-vCPU read
# scored 549 MiB/s with 18k stray WRITE RPCs in its own mountstats, and the
# same rig scored 1840 once the files already existed. A shared `bench.N.0`
# set makes the write run the layout run, so a read measures only reads.
# The stray-write assertion at the bottom of the report is the belt.
FIOCMD="apk add --no-cache fio libaio >/dev/null 2>&1 || true
echo FIO_START=\$(date +%s)
fio --name=bench --directory=/data --rw=$RW --bs=1M --size=4G --numjobs=4 \
    --ioengine=libaio --iodepth=32 --direct=1 --time_based --runtime=$DUR \
    --group_reporting --output-format=json
echo FIO_END=\$(date +%s)"

cat >"$OUT/fio-pod.yaml" <<YAML
apiVersion: v1
kind: Pod
metadata:
  name: $POD
spec:
  restartPolicy: Never
  nodeSelector:
    kubernetes.io/hostname: $CLIENT
  containers:
  - name: fio
    image: alpine:3.19
    command: ["sh","-c"]
    args:
    - |
$(printf '%s\n' "$FIOCMD" | sed 's/^/      /')
    volumeMounts:
    - name: data
      mountPath: /data
  volumes:
  - name: data
    persistentVolumeClaim:
      claimName: $PVC
YAML

kubectl apply -f "$OUT/fio-pod.yaml" >/dev/null
kubectl wait --for=condition=Ready "pod/$POD" --timeout=180s >/dev/null 2>&1 \
  || echo "  ! fio pod not Ready — see kubectl describe pod/$POD"
# The pod exits when fio does; wait on completion, not on Ready.
until [ "$(kubectl get pod "$POD" -o jsonpath='{.status.phase}' 2>/dev/null)" \
        != "Running" ]; do sleep 5; done
kubectl logs "$POD" >"$OUT/fio.json" 2>&1

snap after
wait   # let the samplers finish their window

kubectl get pod "$POD" -o jsonpath='{.spec.nodeName}' >"$OUT/placed-on.txt" 2>/dev/null
kubectl delete pod "$POD" --ignore-not-found >/dev/null 2>&1

echo "  raw: $OUT"
"$HERE/pnfs-fanout-report.py" "$OUT"
