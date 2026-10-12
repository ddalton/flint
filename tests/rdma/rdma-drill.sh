#!/usr/bin/env bash
#
# pNFS-over-RDMA drill: the client reaches the MDS over TCP and ONE data
# server over RPC-over-RDMA (prototypes/rpcrdma-ds), on one Linux host.
#
#   kernel client ──TCP──▶ flint-pnfs-mds   (GETDEVICEINFO: netid "rdma")
#         └──────RDMA────▶ rpcrdma-ds        (READ/WRITE by chunk)
#   flint-pnfs-ds (TCP) only keeps the device registered + heartbeating;
#   the MDS's configured endpoint override points clients past it.
#
# Works on Soft-RoCE (rdma_rxe over $IFACE) or a real RoCE/IB card:
# set RDMA_DEV to an existing device and the rxe step is skipped.
#
# PASS needs ALL of, each observed where the bytes land:
#   1. the data read back (O_DIRECT, caches dropped) matches what was written;
#   2. the kernel's transport list (debugfs sunrpc), sampled DURING the
#      read-back, has an `rdma` transport to the DS port (mountstats
#      shows only the MDS's; the DS's is gone once the file closes);
#   3. rpcrdma-ds's own counters moved >= SIZE by RDMA Read (WRITE) and
#      >= SIZE by RDMA Write (READ) — inline bytes do not count;
#   4. the file in rpcrdma-ds's store has the written checksum;
#   5. control: the TCP DS's export and the MDS export hold none of the
#      data — a client that fell back to MDS/TCP I/O also round-trips
#      the checksum, so (1) alone proves nothing.
#   6. the I/O was healthy: a handful of DS connections and full-size
#      READs — not one page per reconnect.
#
# Needs root, built binaries, and nothing else on rdma-cm port 20049
# (stop nfs-server: knfsd's svcrdma holds it when "rdma 20049" is set).
#
#   sudo BIN_DIR=... PROTO_BIN=... IFACE=eth0 tests/rdma/rdma-drill.sh

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BIN_DIR="${BIN_DIR:-$REPO_ROOT/spdk-csi-driver/target/release}"
PROTO_BIN="${PROTO_BIN:-$REPO_ROOT/prototypes/rpcrdma-ds/target/release/rpcrdma-ds}"
IFACE="${IFACE:-eth0}"
RDMA_DEV="${RDMA_DEV:-}"
SIZE_MB="${SIZE_MB:-64}"
RDMA_PORT=20049
MDS_PORT=20490
DS_PORT=20491
DS_CTL_PORT=21491

W="$(mktemp -d /tmp/rdma-drill.XXXXXX)"
MNT="$W/mnt"
STORE="$W/rdma-store"
STATS="$W/rdma-stats"
TCP_DS_EXPORT="$W/tcp-ds"
MDS_EXPORT="$W/mds-exports"
mkdir -p "$MNT" "$STORE" "$TCP_DS_EXPORT" "$MDS_EXPORT"
PIDS=()

cleanup() {
  set +e
  mountpoint -q "$MNT" && umount -f "$MNT"
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  echo "  logs + configs kept in $W"
}
trap cleanup EXIT

die() { echo "✗ $*"; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root (mount, modprobe, rdma link)"
for b in "$BIN_DIR/flint-pnfs-mds" "$BIN_DIR/flint-pnfs-ds" "$PROTO_BIN"; do
  [ -x "$b" ] || die "missing binary $b"
done

IP="$(ip -4 -o addr show "$IFACE" | awk '{print $4}' | cut -d/ -f1 | head -1)"
[ -n "$IP" ] || die "no IPv4 address on $IFACE"

# ── RDMA device ─────────────────────────────────────────────────────
if [ -z "$RDMA_DEV" ]; then
  modprobe rdma_rxe || die "no rdma_rxe module (linux-modules-extra-$(uname -r)?)"
  rdma link show | grep -q "netdev $IFACE" || rdma link add rxe0 type rxe netdev "$IFACE" \
    || die "rdma link add failed"
  RDMA_DEV="$(rdma link show | awk -v i="$IFACE" '$0 ~ "netdev "i {split($2,a,"/"); print a[1]; exit}')"
fi
rdma link show | grep -q "^link $RDMA_DEV/.*state ACTIVE" || die "$RDMA_DEV is not ACTIVE: $(rdma link show)"
modprobe rpcrdma 2>/dev/null || modprobe xprtrdma || die "no client RPC-over-RDMA transport module"
echo "▶ RDMA device $RDMA_DEV on $IFACE ($IP)"

# ── configs ─────────────────────────────────────────────────────────
cat >"$W/mds.yaml" <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: mds
mds:
  bind: { address: "0.0.0.0", port: $MDS_PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers:
    - deviceId: ds-rdma-1
      # The client-path endpoint: RDMA, straight to the prototype.
      endpoint: "rdma://$IP:$RDMA_PORT"
      # The MDS→DS control path stays on the TCP DS.
      controlEndpoint: "127.0.0.1:$DS_CTL_PORT"
      bdevs: [lvol0]
  state: { backend: memory, config: {} }
  ha: { enabled: false, replicas: 1, leaderElection: false, leaseDuration: 15, renewDeadline: 10, retryPeriod: 2 }
  failover: { heartbeatTimeout: 30, policy: recall_affected, gracePeriod: 60 }
exports:
  - path: $MDS_EXPORT
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: info, format: text, components: {} }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
development:
  debug: false
  simulateFailures: { enabled: false, interval: 0, duration: 0 }
  traceRpc: false
  dumpLayouts: false
EOF

cat >"$W/ds.yaml" <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: ds
ds:
  bind: { address: "0.0.0.0", port: $DS_PORT, controlPort: $DS_CTL_PORT }
  deviceId: ds-rdma-1
  mds: { endpoint: "127.0.0.1:50051", heartbeatInterval: 10, registrationRetry: 5, maxRetries: 0 }
  bdevs: [ { name: lvol0, mount_point: $TCP_DS_EXPORT, spdk_volume: lvol0 } ]
  resources: { maxConnections: 100, ioQueueDepth: 32, ioBufferSize: 1048576 }
  performance: { useSpdkIo: false, ioThreads: 2, zeroCopy: false }
exports:
  - path: /
    fsid: 1
    options: [rw, sync]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: info, format: text, components: {} }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
development:
  debug: false
  simulateFailures: { enabled: false, interval: 0, duration: 0 }
  traceRpc: false
  dumpLayouts: false
EOF

# ── servers ─────────────────────────────────────────────────────────
start() { # name, then the command
  local name=$1; shift
  "$@" >"$W/$name.log" 2>&1 &
  PIDS+=($!)
  sleep 1
  kill -0 "${PIDS[-1]}" 2>/dev/null || { tail -20 "$W/$name.log"; die "$name died on startup"; }
}
start rpcrdma-ds env RPCRDMA_STATS="$STATS" "$PROTO_BIN" "$RDMA_PORT" "$STORE"
# PNFS_MODE: see tests/lima/pnfs/smoke.sh — without it the MDS does not
# set USE_PNFS_MDS and the client never asks for a layout.
start mds env PNFS_MODE=mds "$BIN_DIR/flint-pnfs-mds" --config "$W/mds.yaml"
start tcp-ds env PNFS_MODE=ds "$BIN_DIR/flint-pnfs-ds" --config "$W/ds.yaml"
sleep 2
echo "✓ rpcrdma-ds, MDS and TCP DS are up"

# ── I/O ─────────────────────────────────────────────────────────────
mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT "$IP:/" "$MNT" || die "mount failed"
head -c "${SIZE_MB}M" /dev/urandom >"$W/src.bin"
want="$(md5sum <"$W/src.bin" | cut -d' ' -f1)"
dd if="$W/src.bin" of="$MNT/f.bin" bs=1M oflag=direct status=none || die "write failed"
sync; echo 3 >/proc/sys/vm/drop_caches
# The DS transport lives only while a layout holds it — the kernel
# drops the DS connection once the file is closed — so sample the
# kernel's transport list WHILE the read-back runs, not after it.
mountpoint -q /sys/kernel/debug || mount -t debugfs none /sys/kernel/debug
dd if="$MNT/f.bin" bs=1M iflag=direct status=none of="$W/back.bin" &
dd_pid=$!
xprts=""
while kill -0 $dd_pid 2>/dev/null; do
  xprts+="$(for f in /sys/kernel/debug/sunrpc/rpc_xprt/*/info; do tr '\n' ' ' <"$f" 2>/dev/null; echo; done)"$'\n'
  sleep 0.05
done
wait $dd_pid || die "read failed"
got="$(md5sum <"$W/back.bin" | cut -d' ' -f1)"
xprts="$(echo "$xprts" | sed '/^$/d' | sort -u)"
umount "$MNT"

# ── verdict ─────────────────────────────────────────────────────────
PASS=true
check() { if eval "$2"; then echo "  ✓ $1"; else echo "  ✗ $1"; PASS=false; fi; }
stat_of() { sed -n "s/.*\b$1=\([0-9]*\).*/\1/p" "$STATS" 2>/dev/null; }
size=$((SIZE_MB << 20))
store_md5="$(cat "$STORE"/* 2>/dev/null | md5sum | cut -d' ' -f1)"
tcp_bytes="$(du -sb "$TCP_DS_EXPORT" | cut -f1)"
mds_blocks="$(du -s --block-size=1 "$MDS_EXPORT" | cut -f1)"

echo "▶ client transports:"; echo "$xprts" | sed 's/^/    /'
echo "▶ rpcrdma-ds counters: $(cat "$STATS" 2>/dev/null || echo none)"
check "1. read-back matches ($got)" '[ "$got" = "$want" ]'
check "2. client has an rdma transport to port $RDMA_PORT" 'echo "$xprts" | grep -q "netid: rdma.*port:[[:space:]]*$RDMA_PORT"'
check "3a. WRITE payload by RDMA Read >= $size" '[ "$(stat_of rdma_read)" -ge "$size" ] 2>/dev/null'
check "3b. READ payload by RDMA Write >= $size" '[ "$(stat_of rdma_written)" -ge "$size" ] 2>/dev/null'
check "4. rpcrdma-ds store holds the written bytes" '[ "$(ls "$STORE" | wc -l)" = 1 ] && [ "$store_md5" = "$want" ]'
# A run can move every byte over RDMA and still be broken: a client that
# is refused a full layout reads one page per DS connection (seen when
# the DS claimed FILE_SYNC and the MDS never learned the size).
conns="$(grep -c ESTABLISHED "$W/rpcrdma-ds.log")"
check "6a. few DS connections ($conns)" '[ "$conns" -le 4 ]'
check "6b. READs full-size (avg >= 64 KiB over $(stat_of reads) reads)" '[ "$(stat_of reads)" -ge 1 ] && [ $(( $(stat_of rdma_written) / $(stat_of reads) )) -ge 65536 ]'
check "5a. control: TCP DS export holds no data ($tcp_bytes B)" '[ "$tcp_bytes" -lt 65536 ]'
check "5b. control: MDS export allocates no data ($mds_blocks B)" '[ "$mds_blocks" -lt 1048576 ]'

if $PASS; then echo "✓ PASS: pNFS data path over RDMA"; else echo "✗ FAIL"; exit 1; fi
