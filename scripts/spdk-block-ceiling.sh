#!/usr/bin/env bash
# WHAT IS ONE SPDK REACTOR CORE WORTH? Measured, with NFS out of the path.
#
#   ./scripts/spdk-block-ceiling.sh <node> [storage-class] [seconds]
#
# WHY. flint's whole block data plane on a node is a single CPU core, and
# nothing chose that — it fell out of three defaults agreeing:
#
#   spdk_tgt starts with no -m         -> SPDK default reactor mask 0x1
#   ublk_create_target gets {}         -> NULL cpumask means "app cpuset"
#   ublk makes one poll group PER CORE -> exactly one poller thread
#
# So one core polls the NVMe queues, the NVMe-oF TCP transport for replica
# legs, and every ublk queue on the node. Two pending decisions hang off
# whether that core is actually the binding constraint — whether to expose
# a reactor mask (costing a permanently-spinning core on EVERY node in the
# DaemonSet) and whether to raise the NVMe-oF io_unit_size off its 128 KiB
# default (which drags iobuf's matched 132 KiB large_bufsize and ~1 GiB of
# a 2 GiB hugepage budget with it). Neither should be touched on
# reasoning; both are cheap to settle by measurement.
#
# THE THROUGHPUT NUMBER ALONE CANNOT ANSWER IT. A plateau tells you
# something binds, not what. The instrument that discriminates is
# spdk-tgt's OWN CPU, sampled tightly around each fio window:
#
#   plateau + reactor core at ~100%  -> the core binds; a mask is the fix
#   plateau + reactor core well under -> something else binds (NIC, NVMe,
#                                        ext4, fio itself); a mask buys
#                                        nothing and costs a core per node
#
# An SPDK poller spins at 100% whether or not I/O is arriving, so the
# FLOOR is ~1.0 cores and only the RISE above idle is evidence. This
# script measures that floor first, deliberately, and reports the delta.
#
# SAFETY. This never touches an existing volume. It provisions its own
# PVC, writes only inside it, and deletes it. Do NOT repoint this at a
# raw ublk device holding a filesystem — and note that reading a raw thin
# lvol would measure SPDK's unallocated-zero path rather than the disk,
# which is exactly the kind of number that looks like a result.
set -uo pipefail
NODE=${1:?usage: spdk-block-ceiling.sh <node> [storage-class] [seconds]}
SC=${2:-flint-spdk}
DUR=${3:-20}

NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
: "${KUBECONFIG:?set KUBECONFIG}"
POD="blockceil"
PVC="blockceil-pvc"

echo "▶ block-layer ceiling on $NODE (sc=$SC, ${DUR}s per point)"

# ── spdk-tgt CPU on this node, in cores ─────────────────────────────────
# MATCH ON THE COMMAND LINE, NOT THE PROCESS NAME. SPDK renames its main
# thread to the reactor it is running, so `comm` is "reactor_0" and
# `pgrep -x spdk_tgt` matches NOTHING — the first run of this script
# reported 0.00 cores for every single point, floor included, and a
# zero that looks like an answer is worse than an error.
tgt_cpu() {
  "$HERE/nodesh.sh" "$NODE" \
    'p=$(pgrep -f "/usr/local/bin/spdk_tgt" | head -1); [ -n "$p" ] || { echo 0; exit; }
     awk "{print \$14+\$15}" /proc/$p/stat' 2>/dev/null | tail -1
}

# AND KNOW WHAT THIS NUMBER CANNOT TELL YOU. An SPDK reactor BUSY-POLLS:
# measured on runaz, `ps` shows reactor_0 at 100% of a core while the node
# is completely idle. So CPU% reads ~1.00 whether the reactor is saturated
# or doing nothing, and the "rise above floor" this script was built
# around cannot discriminate the two. It is kept because a rise ABOVE one
# core would still be meaningful (more reactors, or non-reactor threads),
# but the honest instrument for saturation is SPDK's own accounting —
# `thread_get_stats` returns busy_tsc and idle_tsc per thread, and their
# ratio is the utilisation. rpc.py is not in the shipped image, so that
# needs the node agent's RPC passthrough. Until then, treat a flat
# throughput plateau across block size AND queue depth as the evidence
# (that shape means a fixed pipe, not a latency limit) and do not claim
# which component it is without the tsc ratio.

echo -n "  spdk-tgt idle floor: "
F0=$(tgt_cpu); sleep 5; F1=$(tgt_cpu)
FLOOR=$(python3 -c "print(f'{(${F1:-0}-${F0:-0})/100/5:.2f}')")
echo "$FLOOR cores (a poller spins by design — only the RISE is evidence)"

cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: $PVC}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: $SC
  resources: {requests: {storage: 32Gi}}
YAML

kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $POD}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $NODE}
  containers:
  - name: fio
    image: alpine:3.19
    command: ["sh","-c","apk add --no-cache fio libaio >/dev/null 2>&1; sleep 100000"]
    volumeMounts: [{name: d, mountPath: /data}]
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: $PVC}
YAML
kubectl wait --for=condition=Ready "pod/$POD" --timeout=300s >/dev/null 2>&1 \
  || { echo "  ! pod not Ready"; exit 1; }
# fio needs a laid-out file; --direct=1 afterwards then bypasses the page
# cache, so every I/O really traverses ext4 -> ublk -> SPDK -> NVMe.
kubectl exec "$POD" -- sh -c \
  'fio --name=lay --directory=/data --rw=write --bs=1M --size=8G \
       --ioengine=libaio --iodepth=32 --direct=1 --output-format=terse >/dev/null 2>&1' \
  >/dev/null 2>&1

run() {  # rw, bs, iodepth, numjobs
  local a b cpu res mibps
  a=$(tgt_cpu)
  # --size is PER JOB in fio, so numjobs=4 --size=8G wants 32 GiB of files
  # on a 32Gi PVC that already holds the 8 GiB layout file -> ENOSPC, and
  # the point comes back "?" (lost the only multi-job attribution point on
  # the first runaz run). Divide the footprint by the job count, and sum
  # every job's bandwidth rather than reading jobs[0], which is one job's
  # share and would under-report a multi-job run by 4x.
  local per=$((8 / $4))
  res=$(kubectl exec "$POD" -- sh -c \
    "fio --name=lay --directory=/data --rw=$1 --bs=$2 --iodepth=$3 --numjobs=$4 \
         --ioengine=libaio --direct=1 --time_based --runtime=$DUR --size=${per}G \
         --group_reporting --output-format=json 2>/dev/null" \
    | python3 -c "
import sys,json
d=json.load(sys.stdin)
k='read' if '$1'=='read' else 'write'
print(int(sum(j[k]['bw_bytes'] for j in d['jobs'])/1048576))
" 2>/dev/null)
  b=$(tgt_cpu)
  cpu=$(python3 -c "print(f'{(${b:-0}-${a:-0})/100/$DUR:.2f}')")
  printf "  %-6s bs=%-5s qd=%-4s jobs=%-3s %6s MiB/s   spdk-tgt %s cores (+%s over floor)\n" \
    "$1" "$2" "$3" "$4" "${res:-?}" "$cpu" \
    "$(python3 -c "print(f'{max(0,float($cpu)-float($FLOOR)):.2f}')")"
}

echo
echo "── block size sweep (qd=32, 1 job) ──"
for bs in 4k 64k 256k 1M; do run read "$bs" 32 1; done
echo
echo "── concurrency sweep (bs=1M) ──"
for qd in 8 32 128; do run read 1M "$qd" 1; done
run read 1M 32 4
echo
echo "── writes (full stack: ext4 -> ublk -> SPDK -> NVMe) ──"
for bs in 64k 1M; do run write "$bs" 32 1; done

echo
echo "READING THIS: if throughput plateaus while spdk-tgt stays near the"
echo "$FLOOR-core floor, the reactor core is NOT the constraint and a wider"
echo "mask would cost a core per node for nothing. If it plateaus with"
echo "spdk-tgt pinned near 1.00 cores, the single reactor IS the ceiling."

kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
kubectl delete pvc "$PVC" --ignore-not-found >/dev/null 2>&1
