#!/usr/bin/env bash
# ceilings.sh -- the two ceilings every result is read against (plan §5):
#   1. network: iperf3 between nodes over the HOST network (what NVMe-oF
#      replica legs use), long enough to drain "up to" burst credits;
#   2. disk: the fio matrix on each node's raw instance-store NVMe.
#
# DESTRUCTIVE: the disk ceiling WRITES the raw instance-store device. Run it
# before any storage driver initializes the disk, never after.
#
#   OUT=results/ceilings CONFIRM_RAW_WRITE=yes ./ceilings.sh
#
# Env: OUT (required, must not exist)  CONFIRM_RAW_WRITE=yes (required)
#      NET_SECONDS (900)  NET_STREAMS (4)  RUNTIME (60)  RAMP (10)
#      DISK_MODEL (Amazon EC2 NVMe Instance Storage)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
: "${OUT:?set OUT}"
[ "${CONFIRM_RAW_WRITE:-}" = yes ] || { echo "set CONFIRM_RAW_WRITE=yes: this writes each node's raw instance store" >&2; exit 1; }
[ ! -e "$OUT" ] || { echo "$OUT exists" >&2; exit 1; }
NET_SECONDS="${NET_SECONDS:-900}" NET_STREAMS="${NET_STREAMS:-4}"
RUNTIME="${RUNTIME:-60}" RAMP="${RAMP:-10}"
DISK_MODEL="${DISK_MODEL:-Amazon EC2 NVMe Instance Storage}"
NS=flint-bench
mkdir -p "$OUT"
step() { printf '\n▶ %s\n' "$*" >&2; }

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
nodes=$(kubectl get nodes -o jsonpath='{.items[*].metadata.name}')
kubectl get nodes -o wide > "$OUT/nodes.txt"

# One privileged hostNetwork pod per node: iperf3 + fio + nvme-cli.
for n in $nodes; do
  kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: {name: ceil-$n, namespace: $NS, labels: {app: bench-ceil}}
spec:
  nodeName: $n
  hostNetwork: true
  tolerations: [{operator: Exists}]
  restartPolicy: Never
  containers:
  - name: c
    image: alpine:3.20
    command: ["sh", "-c", "apk add --no-cache iperf3 fio nvme-cli >/dev/null && touch /tmp/ready && sleep infinity"]
    readinessProbe: {exec: {command: ["test", "-f", "/tmp/ready"]}, periodSeconds: 2}
    securityContext: {privileged: true}
    volumeMounts: [{name: dev, mountPath: /dev}]
  volumes:
  - name: dev
    hostPath: {path: /dev}
EOF
done
kubectl -n "$NS" wait --for=condition=Ready pod -l app=bench-ceil --timeout=600s >/dev/null

set -- $nodes
a=$1 b=$2
ip_b=$(kubectl get node "$b" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
step "network: $a -> $b ($ip_b), $NET_STREAMS streams, $NET_SECONDS s (burst drain)"
kubectl -n "$NS" exec "ceil-$b" -- sh -c 'pkill iperf3; iperf3 -s -D -p 5201'
sleep 2
kubectl -n "$NS" exec "ceil-$a" -- iperf3 -c "$ip_b" -p 5201 -P "$NET_STREAMS" -t "$NET_SECONDS" -i 60 -J \
  > "$OUT/iperf3-$a-$b.json"
kubectl -n "$NS" exec "ceil-$b" -- pkill iperf3 || true

for n in $nodes; do
  dev=$(kubectl -n "$NS" exec "ceil-$n" -- sh -c "for d in /sys/block/nvme*n1; do m=\$(cat \$d/device/model 2>/dev/null | sed 's/ *\$//'); [ \"\$m\" = '$DISK_MODEL' ] && echo /dev/\$(basename \$d); done" | head -1)
  [ -n "$dev" ] || { echo "no '$DISK_MODEL' device on $n" >&2; exit 1; }
  echo "$n $dev" >> "$OUT/disks.txt"
  step "disk: $n $dev (raw), the matrix at ${RUNTIME}s"
  while read -r name rw bs qd nj mix; do
    case "$name" in ''|'#'*) continue ;; esac
    extra=""; [ "$mix" = - ] || extra="--rwmixread=$mix"
    mkdir -p "$OUT/disk-$n/$name"
    kubectl -n "$NS" exec "ceil-$n" -- fio --name="$name" --filename="$dev" --direct=1 --ioengine=libaio \
      --rw="$rw" --bs="$bs" --iodepth="$qd" --numjobs="$nj" $extra --group_reporting \
      --time_based --runtime="$RUNTIME" --ramp_time="$RAMP" --randrepeat=0 \
      --percentile_list=50:99:99.9 --output-format=json > "$OUT/disk-$n/$name/fio.json"
  done < "$HERE/matrix.tsv"
done

kubectl -n "$NS" delete pod -l app=bench-ceil --wait=false >/dev/null
python3 - "$OUT" <<'PY'
import json, sys, pathlib
out = pathlib.Path(sys.argv[1])
for f in sorted(out.glob("iperf3-*.json")):
    j = json.loads(f.read_text())
    ints = [i["sum"]["bits_per_second"] / 1e9 for i in j["intervals"]]
    print(f"{f.name}: per-minute Gbit/s " + " ".join(f"{x:.2f}" for x in ints)
          + f"; mean {j['end']['sum_received']['bits_per_second']/1e9:.2f}")
for d in sorted(out.glob("disk-*")):
    for t in sorted(p for p in d.iterdir() if p.is_dir()):
        job = json.loads((t / "fio.json").read_text())["jobs"][0]
        iops = job["read"]["iops"] + job["write"]["iops"]
        mib = (job["read"]["bw_bytes"] + job["write"]["bw_bytes"]) / 2**20
        print(f"{d.name} {t.name}: {iops:,.0f} IOPS {mib:,.0f} MiB/s")
PY
