#!/usr/bin/env bash
#
# pNFS multipath trunking drill.
#
# Each DS advertises TWO distinct host IPs (the Lima gateway alias
# 192.168.5.2 and the Mac's LAN IP) via a comma-separated configured
# endpoint. The kernel client must:
#
#   1. Mount and read a striped file successfully (the trunk probe —
#      EXCHANGE_ID on the second address — must accept our replies).
#   2. Hold OPEN TCP connections to BOTH IPs of BOTH DS ports during
#      the read (1 Hz-class union sampling; conns die at file close,
#      so point-in-time checks race — instrument bug #15).
#
# The LAN IP is discovered at run time, so this drill is not part of
# the always-green set — it SKIPs (exit 0 with a notice) when the Mac
# has no usable second IP or the VM cannot reach it.
#
# Exit: 0 PASS/SKIP, 1 FAIL.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
GW_IP=192.168.5.2
MDS_PORT=20490

DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

cleanup() {
  set +e
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
  limactl shell "$LIMA_VM" -- sudo umount -lf /mnt/flint-pnfs 2>/dev/null || true
}
trap cleanup EXIT

echo "▶ pNFS multipath trunking drill"

# ── Second host IP ────────────────────────────────────────────────────
LAN_IP=""
for ifc in en0 en1 en2; do
  ip=$(ipconfig getifaddr "$ifc" 2>/dev/null || true)
  if [ -n "$ip" ] && [ "$ip" != "$GW_IP" ]; then LAN_IP=$ip; break; fi
done
if [ -z "$LAN_IP" ]; then
  echo "△ SKIP: no LAN IP on en0/en1/en2 — multipath needs two distinct host IPs"
  exit 0
fi
echo "  gateway IP: $GW_IP   LAN IP: $LAN_IP"

# ── Rig with multipath endpoints ──────────────────────────────────────
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

MP_CFG=/tmp/flint-pnfs-mds-multipath.yaml
sed -e "s|endpoint: \"$GW_IP:20491\"|endpoint: \"$GW_IP:20491,$LAN_IP:20491\"|" \
    -e "s|endpoint: \"$GW_IP:20492\"|endpoint: \"$GW_IP:20492,$LAN_IP:20492\"|" \
    "$CFG_DIR/mds.yaml" > "$MP_CFG"
if ! grep -q "$LAN_IP:20491" "$MP_CFG"; then
  echo "✗ FAIL: could not inject multipath endpoints into mds.yaml (endpoint lines changed?)"
  exit 1
fi

PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$MP_CFG" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
sleep 1
for n in 1 2; do
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds${n}.yaml" \
    >"$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
done
sleep 2
if ! pgrep -f flint-pnfs-mds >/dev/null; then
  echo "✗ FAIL: MDS died on startup"; tail -20 "$LOG_DIR/flint-pnfs-mds.log"; exit 1
fi

# F68b gate must have accepted both DSes (it dials ALL endpoints incl.
# the LAN IP — if the Mac firewall blocks it, registration NACKs and
# this hangs empty).
sleep 2
if ! grep -q "DS registered successfully" "$LOG_DIR/flint-pnfs-mds.log"; then
  echo "✗ FAIL: no DS registered — F68b gate rejecting? MDS log:"
  grep -E "F68b|Registration" "$LOG_DIR/flint-pnfs-mds.log" | tail -6
  exit 1
fi

# ── VM reachability of the LAN IP ─────────────────────────────────────
if ! limactl shell "$LIMA_VM" -- timeout 3 bash -c "exec 3<>/dev/tcp/$LAN_IP/20491" 2>/dev/null; then
  echo "△ SKIP: VM cannot reach $LAN_IP:20491 (NAT/firewall) — trunking untestable on this host"
  exit 0
fi
echo "✓ VM reaches both host IPs"

# ── Mount, read, union-sample connections ─────────────────────────────
RESULT=$(limactl shell "$LIMA_VM" -- sudo bash -s "$GW_IP" "$LAN_IP" <<'EOS'
set -u
GW=$1; LAN=$2
M=/mnt/flint-pnfs
mkdir -p $M
umount -lf $M 2>/dev/null
# nconnect>=2 is REQUIRED for DS trunking: nfs4_set_ds_client only
# raises the DS client's max_connect above 1 when the MDS client has
# cl_nconnect > 1 — a default mount silently refuses every trunk
# candidate (kernel nfs4client.c, v6.1-6.8).
mount -t nfs4 -o minorversion=1,proto=tcp,port=20490,nconnect=4 host.lima.internal:/ $M || { echo VERDICT=MOUNT_FAIL; exit 0; }

dd if=/dev/urandom of=$M/mp.bin bs=1M count=512 oflag=direct status=none || { echo VERDICT=WRITE_FAIL; exit 0; }

# Read in the background; union-sample sockets until it exits.
dd if=$M/mp.bin of=/dev/null bs=1M iflag=direct status=none &
DD=$!
: > /tmp/mp.samples
while kill -0 $DD 2>/dev/null; do
  ss -tn 2>/dev/null | awk '{print $5}' >> /tmp/mp.samples
  sleep 0.2
done
wait $DD

seen() { grep -c "^$1:$2$" /tmp/mp.samples 2>/dev/null || true; }
echo "conns GW:20491=$(seen $GW 20491) LAN:20491=$(seen $LAN 20491) GW:20492=$(seen $GW 20492) LAN:20492=$(seen $LAN 20492)"
ok=1
for pair in "$GW 20491" "$LAN 20491" "$GW 20492" "$LAN 20492"; do
  [ "$(seen $pair)" -gt 0 ] || ok=0
done
umount $M
[ $ok = 1 ] && echo VERDICT=PASS || echo VERDICT=NO_TRUNK
EOS
)
echo "$RESULT"

case "$RESULT" in
  *VERDICT=PASS*)
    echo "✓ PASS: client opened transports to BOTH IPs of BOTH DSes during the read"
    exit 0 ;;
  *VERDICT=NO_TRUNK*)
    echo "✗ FAIL: mount worked but the client did not trunk to the second address"
    echo "  (check EXCHANGE_ID replies are identical across DS addresses — trunk probe)"
    exit 1 ;;
  *)
    echo "✗ FAIL: $RESULT"
    exit 1 ;;
esac
