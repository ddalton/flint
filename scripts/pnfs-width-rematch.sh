#!/usr/bin/env bash
# THE c6gn WIDTH REMATCH — does pNFS width scale past one DS's 24 Gbps
# when the client NIC is 100 Gbps? (runbd: width-2 FLAT at ~2810 because
# ONE rc3 DS saturates a 25 Gbps client NIC; runbe's c6gn "rematch" was
# INVALIDATED — every byte went through the MDS proxy, F68.)
#
#   KUBECONFIG=... ./scripts/pnfs-width-rematch.sh <client-node>
#
# Fleet expectations: 2 DS pods + 1 MDS pod, none on <client-node>,
# WireGuard OFF. The drill HARD-FAILS (no numbers) unless:
#   - every DS/MDS pod sits on a node other than the client's;
#   - Cilium WireGuard is off (the 918 tunnel tax voids ceilings);
#   - right after EVERY timed I/O the client holds established TCP to
#     EACH DS (per-pod svc ClusterIP OR pod IP — socket-LB rewrites
#     the dst at connect(), so ss shows pod IPs; conn topology is the
#     truthful instrument, the LAYOUTGET mountstats row is INERT on
#     6.1, and DS conns linger minutes so post-transfer is race-free);
#   - the client's PHYSICAL NIC moved ≥80% of the payload bytes in the
#     transfer direction (catches cache-served "reads" — runbc's 2432 —
#     and is immune to the vxlan double-count, instrument bug #10,
#     because it reads ONLY the default-route interface).
#
# Protocol (runay window rule: one session, both directions, re-baseline
# at the end):
#   w2 write → w2 read → w1 write → w1 read → w2 read re-baseline
# Widths come from per-SC stripeWidth (fleet stays at 2 DSes); each arm
# uses its own PVC. Reads are cold: node page cache dropped via a
# privileged helper pod before every timed read. Timing is busybox dd's
# OWN elapsed-seconds report (no kubectl-exec setup skew). Runs from a
# macOS bash 3.2 without apology.
set -uo pipefail
CLIENT=${1:?usage: pnfs-width-rematch.sh <client-node>}
NS=${FLINT_NS:-flint-system}
OUT=${OUT_DIR:-/tmp/pnfs-width-rematch}
GIB=${GIB:-16}                # per timed transfer
WIDTHS=${WIDTHS:-"2 1"}       # arm order; first width re-baselined last
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

ts()  { date +%H:%M:%S; }
say() { printf "[%s] %s\n" "$(ts)" "$*"; }
die() { printf "[%s] ✗ ABORT: %s\n" "$(ts)" "$*"; exit 1; }

# ── preflight: topology ──────────────────────────────────────────────
# Each DS is identified by BOTH its per-pod Service ClusterIP and its
# pod IP: cilium's socket-LB rewrites the destination at connect(), so
# `ss` on the client shows the POD IP, never the ClusterIP the kernel
# was handed (instrument bug #14 — the ClusterIP-only version of this
# assertion void-aborted a healthy DS-direct run on runbg).
DS_PAIRS=""
while read -r pod node podip; do
  [ -n "$pod" ] || continue
  [ "$node" != "$CLIENT" ] || die "DS $pod is ON the client node $CLIENT"
  ip=$(kubectl get svc -n "$NS" "$pod" -o jsonpath='{.spec.clusterIP}') \
    || die "no per-pod svc for $pod"
  DS_PAIRS="$DS_PAIRS $ip|$podip"
  say "✓ $pod on $node, svc $ip, pod $podip"
done <<EOF
$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.metadata.name} {.spec.nodeName} {.status.podIP}{"\n"}{end}')
EOF
[ -n "$DS_PAIRS" ] || die "no DS pods"
NDS=$(echo "$DS_PAIRS" | wc -w | tr -d ' ')

MDS_NODE=$(kubectl get pods -n "$NS" -l chert.us/role=pnfs-mds \
  -o jsonpath='{.items[0].spec.nodeName}' 2>/dev/null)
[ -n "$MDS_NODE" ] || MDS_NODE=$(kubectl get pods -n "$NS" -o wide \
  | awk '/pnfs-mds/{print $7; exit}')
[ -n "$MDS_NODE" ] || die "no MDS pod found"
[ "$MDS_NODE" != "$CLIENT" ] || die "MDS is ON the client node (NIC math void)"
say "✓ MDS on $MDS_NODE, client is $CLIENT ($NDS DSes)"

WG=$(kubectl get cm -n kube-system cilium-config \
  -o jsonpath='{.data.enable-wireguard}' 2>/dev/null)
[ "$WG" != "true" ] || die "Cilium WireGuard is ON — disable before measuring"
say "✓ WireGuard off (enable-wireguard='${WG:-unset}')"

# ── helper pod on the client node (privileged, host netns/PID) ───────
kubectl delete pod wr-nodeadmin --ignore-not-found --wait=true >/dev/null 2>&1
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: wr-nodeadmin}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  hostNetwork: true
  hostPID: true
  tolerations: [{operator: Exists}]
  containers:
  - name: a
    image: alpine:3.20
    command: ["sh","-c","apk add --no-cache iproute2 >/dev/null 2>&1; sleep 200000"]
    securityContext: {privileged: true}
YAML
kubectl wait --for=condition=Ready pod/wr-nodeadmin --timeout=180s >/dev/null \
  || die "nodeadmin pod not Ready on $CLIENT"
nadm() { kubectl exec wr-nodeadmin -- sh -c "$1"; }
drop_caches() { nadm "sync; echo 3 > /proc/sys/vm/drop_caches"; }

# Physical NIC = default-route interface in the host netns. Reading only
# this interface sidesteps veth/vxlan double-counting (bug #10).
NIC=$(nadm "ip route show default | head -1 | sed 's/.* dev \([^ ]*\).*/\1/'")
[ -n "$NIC" ] || die "cannot resolve default-route NIC on $CLIENT"
say "✓ client physical NIC: $NIC"
nic_bytes() { # rx|tx
  nadm "cat /sys/class/net/$NIC/statistics/$1_bytes"
}

# ── the path assertion (F68a, drill side) ────────────────────────────
# A 1 Hz ss sampler runs in the nodeadmin pod for the WHOLE transfer
# and the assertion checks the UNION of endpoints seen. Point-in-time
# sampling loses both ways: buffered writes open their first DS conn
# only when writeback starts (~6s in), and the kernel tears DS conns
# down at file close (every runbg trace re-ran _nfs4_pnfs_v4_ds_connect
# [new]) — so mid-flight races the start and post-transfer races the
# end.
sampler_start() {
  nadm "rm -f /tmp/wr.samples /tmp/wr.stop
cat > /tmp/wr-sampler.sh <<'EOS'
#!/bin/sh
i=0
while [ \$i -lt 900 ] && [ ! -f /tmp/wr.stop ]; do
  ss -tn state established 2>/dev/null | awk '{print \$4}' >> /tmp/wr.samples
  i=\$((i+1)); sleep 1
done
EOS
nohup sh /tmp/wr-sampler.sh >/dev/null 2>&1 &
echo sampler-up"
}
sampler_stop() { nadm "touch /tmp/wr.stop; sleep 1.2; sort -u /tmp/wr.samples 2>/dev/null"; }

assert_ds_direct() {  # label union-of-endpoints — every DS (svcIP or podIP) must appear
  local label=$1 union=$2 missing=0 c pair svc pod
  for pair in $DS_PAIRS; do
    svc=${pair%|*}; pod=${pair#*|}
    c=$(echo "$union" | grep -cE "^(${svc}|${pod}):2049$")
    if [ "$c" -eq 0 ]; then missing=1; say "  ✗ NO conns to DS ${svc}/${pod}:2049 in any sample"; fi
  done
  if [ "$missing" = 1 ]; then
    echo "$union" > "$OUT/$label.conns"
    die "$label: NOT DS-direct (proxy regime? see $OUT/$label.conns) — number VOID"
  fi
  say "  ✓ $label: DS-direct confirmed ($NDS DS endpoints seen during transfer)"
}

# ── per-width SC + PVC + consumer pod ────────────────────────────────
mk_sc() {  # width
  kubectl get sc "flint-pnfs-w$1" >/dev/null 2>&1 && return
  kubectl get sc flint-pnfs -o json | python3 -c "
import json,sys
sc=json.load(sys.stdin)
sc['metadata']={'name':'flint-pnfs-w$1'}
sc.setdefault('parameters',{})['stripeWidth']='$1'
sc['reclaimPolicy']='Delete'
print(json.dumps(sc))" | kubectl apply -f - >/dev/null || die "SC clone w$1"
  say "✓ SC flint-pnfs-w$1"
}
mk_consumer() {  # width
  cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: wr-w$1}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint-pnfs-w$1
  resources: {requests: {storage: 64Gi}}
---
apiVersion: v1
kind: Pod
metadata: {name: wr-c$1}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  containers:
  - name: b
    image: alpine:3.20
    command: ["sh","-c","sleep 200000"]
    volumeMounts: [{name: d, mountPath: /data}]
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: wr-w$1}
YAML
  kubectl wait --for=condition=Ready "pod/wr-c$1" --timeout=300s >/dev/null \
    || die "consumer wr-c$1 not Ready (CSI driver on $CLIENT?)"
  local landed
  landed=$(kubectl get pod "wr-c$1" -o jsonpath='{.spec.nodeName}')
  [ "$landed" = "$CLIENT" ] || die "consumer landed on $landed"
}

timed() {  # label pod dd-cmd nic-direction(rx|tx)
  local label=$1 pod=$2 cmd=$3 dir=$4 out secs rate b0 b1 moved want union
  sampler_start >/dev/null
  b0=$(nic_bytes "$dir")
  out=$(kubectl exec "$pod" -- sh -c "$cmd" 2>&1) \
    || die "$label I/O failed: $out"
  b1=$(nic_bytes "$dir")
  union=$(sampler_stop)
  assert_ds_direct "$label" "$union"
  secs=$(echo "$out" | awk '/copied,/{print $(NF-2)}')
  [ -n "$secs" ] || die "$label: no dd timing in: $out"
  rate=$(awk -v g="$GIB" -v s="$secs" 'BEGIN{printf "%.0f", g*1024/s}')
  moved=$(( (b1 - b0) / 1048576 ))
  want=$(( GIB * 1024 * 80 / 100 ))
  if [ "$moved" -lt "$want" ]; then
    die "$label: NIC $dir moved only ${moved} MiB of ${GIB} GiB — cache/local artifact, number VOID"
  fi
  say "▷ $label: ${rate} MiB/s (dd ${secs}s, NIC $dir ${moved} MiB)"
  echo "$label $rate MiB/s  nic-${dir} ${moved} MiB" >> "$OUT/results.txt"
}

# ── arms ─────────────────────────────────────────────────────────────
: > "$OUT/results.txt"
for W in $WIDTHS; do
  mk_sc "$W"; mk_consumer "$W"
  say "── width $W ──"
  timed "w${W}-write" "wr-c$W" \
    "dd if=/dev/zero of=/data/f.bin bs=1M count=$((GIB*1024)) conv=fsync" tx
  drop_caches
  timed "w${W}-read" "wr-c$W" \
    "dd if=/data/f.bin of=/dev/null bs=1M" rx
done
FIRST=$(echo "$WIDTHS" | awk '{print $1}')
drop_caches
timed "w${FIRST}-read-rebaseline" "wr-c$FIRST" \
  "dd if=/data/f.bin of=/dev/null bs=1M" rx

say "── results ──"
column -t "$OUT/results.txt"
say "artifacts in $OUT  (cleanup: kubectl delete pod wr-nodeadmin wr-c1 wr-c2; kubectl delete pvc wr-w1 wr-w2)"
