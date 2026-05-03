#!/usr/bin/env bash
#
# pNFS single-host nconnect sweep — exposes whether per-TCP-serial RPC
# handling at `server_v4.rs:176` is the throughput ceiling on this
# hardware.
#
# Linux NFSv4.1 mounts can open up to 16 TCP connections per session
# via the `nconnect=N` mount option; each connection has its own slot
# table on the same session. Our forward-channel RPC handler processes
# RPCs sequentially per connection (read frame → dispatch → reply →
# repeat). If that's the bottleneck on this hardware, throughput
# climbs as nconnect grows. If throughput is flat across nconnect,
# the bottleneck is elsewhere (storage, kernel, loopback).
#
# What this DOES expose
#   * Whether more parallel TCP connections help — i.e. whether the
#     per-TCP-serial RPC loop in server_v4.rs is the single-host
#     ceiling on this Mac.
#   * The shape of that scaling vs. block size: at bs=4K every byte
#     pays per-RPC overhead, so per-TCP serialisation matters a lot;
#     at bs=1M each RPC moves more data so the relative cost shrinks.
#   * Whether reads scale differently from writes (they probably do —
#     on this hardware reads tie at ~270 MiB/s due to loopback TCP
#     saturation, regardless of pNFS striping).
#
# What this DOES NOT expose
#   * Cross-host scaling. MDS + 2 DSes + client all share one macOS
#     kernel + APFS journal; loopback TCP serialises everything.
#     The architectural promise of N× scaling with N DSes on N nodes
#     stays a prediction until a real cluster bench runs.
#   * Real production read throughput. The Mac's page cache is warm
#     after the WRITE phase; we drop the *client* cache before
#     reading, but the server-side host cache is hot.
#
# Sweep dimensions
#   nconnect = 1, 4, 8, 16
#   bs       = 4K, 1M
#   rw       = read, write
#   jobs     = 4 (fixed — adjust JOBS_OVERRIDE env if needed)
#
# Total: 16 fio runs ≈ 5–10 min wall time.
#
# Output
#   * Stdout: a markdown table per (bs, rw) showing MiB/s vs nconnect.
#   * JSON: per-run fio output saved to /tmp/flint-pnfs-nconnect-*.json
#     so the curve can be replotted later.
#   * Exit 0 always — this is exploratory, not pass/fail. The
#     question we care about is "what's the shape of the curve?",
#     not "did it cross 1.3×".

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"

MDS_PORT=20490
DS1_PORT=20491
DS2_PORT=20492
DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"

VM_MOUNT="/mnt/flint-bench"

# Per-job size in MiB. Smaller than bench.sh's 256 because we run 16
# phases, not 2; staying inside /tmp's free space matters.
SIZE_MB="${SIZE_MB_OVERRIDE:-128}"
JOBS="${JOBS_OVERRIDE:-4}"

# The sweep itself. NCONNECT_VALUES intentionally includes 1 so we
# can compare against single-connection (== flint-pnfs's default
# without explicit nconnect) — that's the baseline the curve is
# measured against.
NCONNECT_VALUES="${NCONNECT_VALUES_OVERRIDE:-1 4 8 16}"
BS_VALUES="${BS_VALUES_OVERRIDE:-4K 1M}"

cleanup() {
  set +e
  limactl shell "$LIMA_VM" -- sudo umount -lf "$VM_MOUNT" 2>/dev/null
  pkill -f flint-pnfs-mds 2>/dev/null
  pkill -f flint-pnfs-ds  2>/dev/null
  rm -f /tmp/flint-pnfs-{mds,ds1,ds2}.pid
}
trap cleanup EXIT

echo "▶ pNFS single-host nconnect sweep"
echo "  workload:    $JOBS jobs × ${SIZE_MB} MiB"
echo "  nconnect:    $NCONNECT_VALUES"
echo "  block sizes: $BS_VALUES"
echo "  modes:       write, read"
echo

# ──────────────────────────────────────────────────────────────────────
# Pre-flight
# ──────────────────────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || { echo "✗ Missing binary: $BIN_DIR/$bin"
    echo "  Run: cd spdk-csi-driver && cargo build --release"; exit 1; }
done

if ! limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM"; then
  echo "✗ Lima VM '$LIMA_VM' not running. Run: make lima-up"; exit 1
fi
if ! limactl shell "$LIMA_VM" -- bash -lc 'command -v fio' >/dev/null 2>&1; then
  echo "▶ installing fio in Lima VM (one-time)"
  limactl shell "$LIMA_VM" -- sudo apt-get -qq -y install fio >/dev/null 2>&1
fi
limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT"

# ──────────────────────────────────────────────────────────────────────
# Bring up MDS + 2 DSes (we keep one set running for the whole sweep
# — bringing them up + down per nconnect value would dominate wall
# time and add noise from cold caches.)
# ──────────────────────────────────────────────────────────────────────
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  > "$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > /tmp/flint-pnfs-mds.pid
sleep 1
kill -0 "$(cat /tmp/flint-pnfs-mds.pid)" 2>/dev/null \
  || { echo "✗ MDS died"; tail -20 "$LOG_DIR/flint-pnfs-mds.log"; exit 1; }
echo "  ✓ MDS up on port $MDS_PORT"

for n in 1 2; do
  cfg="$CFG_DIR/ds${n}.yaml"
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$cfg" \
    > "$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "/tmp/flint-pnfs-ds${n}.pid"
done
sleep 2
for n in 1 2; do
  kill -0 "$(cat /tmp/flint-pnfs-ds${n}.pid)" 2>/dev/null \
    || { echo "✗ DS${n} died"; tail -20 "$LOG_DIR/flint-pnfs-ds${n}.log"; exit 1; }
done
echo "  ✓ DS1, DS2 up; waiting 3s for first heartbeat to land in MDS"
sleep 3
echo

# ──────────────────────────────────────────────────────────────────────
# Helpers
# ──────────────────────────────────────────────────────────────────────
drop_client_cache() {
  limactl shell "$LIMA_VM" -- sudo bash -c 'sync && echo 3 > /proc/sys/vm/drop_caches' \
    >/dev/null 2>&1
}

# Mount with a specific nconnect. Unmount first if already mounted.
remount() {
  local nconn="$1"
  limactl shell "$LIMA_VM" -- sudo bash -c "
    set -eu
    mountpoint -q $VM_MOUNT && umount -lf $VM_MOUNT || true
    mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT,nconnect=$nconn,rsize=1048576,wsize=1048576 \
      $HOST_ADDR:/ $VM_MOUNT
  " || { echo "    ✗ mount with nconnect=$nconn failed"; return 1; }
}

# Run a single fio phase and emit MiB/s on stdout. JSON saved per phase.
#   $1: nconnect
#   $2: bs (e.g. 4K, 1M)
#   $3: rw (read or write)
fio_phase() {
  local nconn="$1" bs="$2" rw="$3"
  local tag="nc${nconn}-bs${bs}-${rw}"
  local out="$LOG_DIR/flint-pnfs-nconnect-$tag.json"

  drop_client_cache

  # `--directory` drops fio's files into our mount. Same filename
  # pattern across phases lets read-after-write work without a
  # separate prep step.
  limactl shell "$LIMA_VM" -- bash -c "fio \
    --name=bench \
    --directory=$VM_MOUNT \
    --rw=$rw \
    --bs=$bs \
    --numjobs=$JOBS \
    --size=${SIZE_MB}M \
    --ioengine=libaio \
    --iodepth=16 \
    --direct=0 \
    --end_fsync=1 \
    --group_reporting \
    --output-format=json" \
    > "$out" 2>"$LOG_DIR/flint-pnfs-nconnect-$tag.stderr"

  local kbps
  kbps=$(jq -r ".jobs[0].$rw.bw // 0" < "$out")
  awk -v kbps="$kbps" 'BEGIN { printf "%.1f", kbps / 1024 }'
}

# ──────────────────────────────────────────────────────────────────────
# Sweep
# ──────────────────────────────────────────────────────────────────────
# macOS ships bash 3.2 which doesn't support associative arrays, so we
# accumulate `bs|rw|nconn mibs` lines into a temp file and grep them
# back during summary. Same shape, no portability footgun.
RESULTS_FILE="$LOG_DIR/flint-pnfs-nconnect-results.tsv"
: > "$RESULTS_FILE"

for bs in $BS_VALUES; do
  for rw in write read; do
    echo "═══ bs=$bs rw=$rw ═══"
    for nconn in $NCONNECT_VALUES; do
      printf "  nconnect=%-2d " "$nconn"
      if remount "$nconn"; then
        mibs=$(fio_phase "$nconn" "$bs" "$rw")
      else
        mibs="MOUNT_FAIL"
      fi
      printf "%s\t%s\t%s\t%s\n" "$bs" "$rw" "$nconn" "$mibs" >> "$RESULTS_FILE"
      printf "%8s MiB/s\n" "$mibs"
    done
    echo
  done
done

# Helper: look up a result by (bs, rw, nconn). Returns "—" if missing.
lookup_result() {
  awk -v bs="$1" -v rw="$2" -v nc="$3" '
    BEGIN { FS="\t" }
    $1==bs && $2==rw && $3==nc { print $4; exit }
  ' "$RESULTS_FILE" 2>/dev/null || true
}

# Final unmount before tearing down servers (cleanup() also handles
# this; doing it here makes the log line ordering nicer).
limactl shell "$LIMA_VM" -- sudo umount -lf "$VM_MOUNT" 2>/dev/null

# ──────────────────────────────────────────────────────────────────────
# Summary tables
# ──────────────────────────────────────────────────────────────────────
echo
echo "════════════════════════════════════════════════════════════════"
echo " pNFS single-host nconnect sweep — MiB/s aggregate"
echo "  (workload: $JOBS jobs × ${SIZE_MB} MiB; macOS host + Lima client; loopback TCP)"
echo "════════════════════════════════════════════════════════════════"

# Print a markdown-flavored table per bs.
for bs in $BS_VALUES; do
  echo
  echo "### bs=$bs"
  echo
  printf "| %-7s |" "nconnect"
  for nconn in $NCONNECT_VALUES; do printf " %7d |" "$nconn"; done
  printf "\n"
  printf "| %-7s |" "-------"
  for nconn in $NCONNECT_VALUES; do printf " %7s |" "-------"; done
  printf "\n"
  for rw in write read; do
    printf "| %-7s |" "$rw"
    for nconn in $NCONNECT_VALUES; do
      val="$(lookup_result "$bs" "$rw" "$nconn")"
      printf " %7s |" "${val:-—}"
    done
    printf "\n"
  done
done

echo
echo "════════════════════════════════════════════════════════════════"
echo " Reading the curve"
echo
echo "  • If MiB/s climbs with nconnect → per-TCP-serial RPC handler"
echo "    at server_v4.rs:176 is the single-host ceiling. Pipeline"
echo "    that loop (~2 weeks) and re-bench. ADR will name the gain."
echo
echo "  • If MiB/s is flat across nconnect → the bottleneck is below"
echo "    the per-connection layer. Likely candidates: kernel page"
echo "    cache writeback, APFS journal contention (server-side),"
echo "    loopback TCP saturation, fio iodepth. Cross-host bench"
echo "    becomes the only way to learn more."
echo
echo "  • If 4K and 1M scale differently → per-RPC overhead is the"
echo "    dominant cost at small bs; bigger payloads amortise it. A"
echo "    common shape: 4K scales steeply with nconnect; 1M plateaus"
echo "    early because the bottleneck is bytes, not RPCs."
echo "════════════════════════════════════════════════════════════════"
echo
echo "  Per-run JSONs in $LOG_DIR/flint-pnfs-nconnect-*.json"
exit 0
