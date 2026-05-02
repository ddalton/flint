#!/usr/bin/env bash
#
# csi-e2e.sh — end-to-end test for the pNFS CSI integration.
#
# This validates the same code path Kubernetes would drive
# (pnfs_csi::create_volume → mount → I/O → unmount → delete_volume),
# without requiring a cluster. The CLI binary `pnfs-csi-cli` exercises
# the gRPC verbs; the shell wrapping handles mount/umount inside the
# Lima VM (because macOS NFSv4.1 doesn't support pNFS layouts).
#
# What it asserts
#   1. CreateVolume returns a volume_context carrying the five
#      pnfs.flint.io/* keys (mds-ip, mds-port, export-path,
#      volume-file, size-bytes).
#   2. Mount with those keys + production mount options succeeds.
#   3. Round-trip data integrity (sha256 written == sha256 read back).
#   4. Both DSes have file content after the run (real striping).
#   5. DeleteVolume removes the file from the MDS export.
#   6. A second CreateVolume after DeleteVolume succeeds (no stale
#      state on the MDS).
#
# Exit 0 = PASS, non-zero = FAIL. Suitable as a Makefile gate.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR="/tmp"
PIDFILE_DIR="/tmp"

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
HOST_ADDR="host.lima.internal"
MDS_PORT=20490
MDS_GRPC_PORT=50051
DS1_EXPORT="/tmp/flint-pnfs-ds1"
DS2_EXPORT="/tmp/flint-pnfs-ds2"
MDS_EXPORT_DIR="/tmp/flint-pnfs-mds-exports"
VM_MOUNT="/mnt/flint-csi-e2e"
VOLUME_ID="pvc-csi-e2e-test"
SIZE_BYTES=$((512 * 1024 * 1024))   # 512 MiB — small but big enough
                                     # to land multiple stripes on
                                     # both DSes (8 MiB stripe).

# ──────────────────────────────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────────────────────────────

cleanup() {
  set +e
  limactl shell "$LIMA_VM" -- sudo umount -lf "$VM_MOUNT" 2>/dev/null
  for n in mds ds1 ds2; do
    if [ -f "$PIDFILE_DIR/flint-pnfs-$n.pid" ]; then
      kill "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null
      rm -f "$PIDFILE_DIR/flint-pnfs-$n.pid"
    fi
  done
  pkill -9 -f "flint-pnfs-mds" 2>/dev/null || true
  pkill -9 -f "flint-pnfs-ds"  2>/dev/null || true
}
trap cleanup EXIT

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '✓ %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

# ──────────────────────────────────────────────────────────────────────
# Pre-flight
# ──────────────────────────────────────────────────────────────────────

step "pre-flight"
for bin in flint-pnfs-mds flint-pnfs-ds pnfs-csi-cli; do
  [ -x "$BIN_DIR/$bin" ] || fail "missing binary: $BIN_DIR/$bin (run: cd spdk-csi-driver && cargo build --release)"
done
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running (make lima-up)"
command -v jq >/dev/null 2>&1 || fail "jq required (brew install jq)"
ok "binaries + Lima VM + jq present"

# ──────────────────────────────────────────────────────────────────────
# Bring up MDS + DSes
# ──────────────────────────────────────────────────────────────────────

step "starting MDS + 2 DSes"
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  > "$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! > "$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
kill -0 "$(cat $PIDFILE_DIR/flint-pnfs-mds.pid)" 2>/dev/null \
  || { tail -20 "$LOG_DIR/flint-pnfs-mds.log"; fail "MDS died on startup"; }

for n in 1 2; do
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds${n}.yaml" \
    > "$LOG_DIR/flint-pnfs-ds${n}.log" 2>&1 &
  echo $! > "$PIDFILE_DIR/flint-pnfs-ds${n}.pid"
done
sleep 2
for n in 1 2; do
  kill -0 "$(cat $PIDFILE_DIR/flint-pnfs-ds${n}.pid)" 2>/dev/null \
    || { tail -20 "$LOG_DIR/flint-pnfs-ds${n}.log"; fail "DS${n} died on startup"; }
done
ok "MDS + DS1 + DS2 are up"

# ──────────────────────────────────────────────────────────────────────
# (1) CreateVolume via the gRPC verb the CSI driver uses
# ──────────────────────────────────────────────────────────────────────

step "CreateVolume($VOLUME_ID, $SIZE_BYTES)"
CTX_JSON=$("$BIN_DIR/pnfs-csi-cli" create \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" \
  --size-bytes "$SIZE_BYTES" 2>&1) || { echo "$CTX_JSON"; fail "CreateVolume failed"; }
echo "  context: $CTX_JSON"

# Assert all five keys are present (the contract from PR 2's
# pnfs_csi::ctx_keys module — if any of these is missing or renamed,
# this test catches it before the CSI driver does in production).
for key in \
    "pnfs.flint.io/mds-ip" \
    "pnfs.flint.io/mds-port" \
    "pnfs.flint.io/export-path" \
    "pnfs.flint.io/volume-file" \
    "pnfs.flint.io/size-bytes"; do
  echo "$CTX_JSON" | jq -e "has(\"$key\")" >/dev/null \
    || fail "volume_context missing required key: $key"
done
ok "volume_context carries all five required keys"

EXPORT_PATH=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/export-path"')
VOLUME_FILE=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/volume-file"')
SIZE_REPORTED=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/size-bytes"')
[ "$SIZE_REPORTED" = "$SIZE_BYTES" ] \
  || fail "size mismatch: requested $SIZE_BYTES, got $SIZE_REPORTED"

# Verify the MDS actually created the file at the right size.
[ -f "$EXPORT_PATH/$VOLUME_FILE" ] \
  || fail "MDS file not present at $EXPORT_PATH/$VOLUME_FILE"
FILE_SIZE=$(stat -f %z "$EXPORT_PATH/$VOLUME_FILE")
[ "$FILE_SIZE" = "$SIZE_BYTES" ] \
  || fail "MDS file size $FILE_SIZE != requested $SIZE_BYTES"
ok "MDS-side file present at correct size"

# ──────────────────────────────────────────────────────────────────────
# (2) Mount with the same options NodePublishVolume uses
# ──────────────────────────────────────────────────────────────────────

step "mount on Lima client"
limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT"

MOUNT_OPTS="minorversion=1,proto=tcp,port=$MDS_PORT,nconnect=4,rsize=1048576,wsize=1048576,noresvport"
limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "$MOUNT_OPTS" \
  "$HOST_ADDR:/" "$VM_MOUNT" \
  || fail "mount failed (see $LOG_DIR/flint-pnfs-mds.log)"
ok "mounted with production options"

# ──────────────────────────────────────────────────────────────────────
# (3) Round-trip data integrity
# ──────────────────────────────────────────────────────────────────────

step "write + sha256 round-trip"
WRITE_HASH=$(limactl shell "$LIMA_VM" -- bash -c "
  set -eu
  # Write 64 MiB of urandom to the volume's file (named after volume_id).
  dd if=/dev/urandom of=$VM_MOUNT/$VOLUME_ID bs=1M count=64 conv=notrunc \
    2>/dev/null
  sync
  sha256sum $VM_MOUNT/$VOLUME_ID | awk '{print \$1}'
")
[ -n "$WRITE_HASH" ] || fail "write hash empty"
ok "wrote 64 MiB, sha256=${WRITE_HASH:0:16}…"

# Drop client cache so the read actually goes through pNFS.
limactl shell "$LIMA_VM" -- sudo bash -c 'sync && echo 3 > /proc/sys/vm/drop_caches' \
  >/dev/null 2>&1

READ_HASH=$(limactl shell "$LIMA_VM" -- bash -c \
  "sha256sum $VM_MOUNT/$VOLUME_ID | awk '{print \$1}'")
[ "$READ_HASH" = "$WRITE_HASH" ] \
  || fail "data corruption: wrote $WRITE_HASH, read back $READ_HASH"
ok "round-trip hash matches"

# ──────────────────────────────────────────────────────────────────────
# (4) Per-DS striping evidence
# ──────────────────────────────────────────────────────────────────────

step "verifying striping"
limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT"

# Each DS should hold the file (sparse). What matters is that BOTH
# DSes have actual block allocations; a fall-back-to-MDS-direct path
# would leave one or both DSes empty.
# macOS BSD `stat -f %b` returns blocks-allocated in 512-byte units
# (matches the kernel's st_blocks). Multiplying by 512 gives bytes
# actually on disk for sparse files. Don't use `%B` — that's the
# *filesystem* block size, unrelated to per-file allocation.
for n in 1 2; do
  ds_dir="/tmp/flint-pnfs-ds${n}"
  [ -f "$ds_dir/$VOLUME_ID" ] || fail "DS${n} has no file for $VOLUME_ID"
  ds_blocks=$(stat -f %b "$ds_dir/$VOLUME_ID")
  ds_alloc=$(( ds_blocks * 512 ))
  [ "$ds_alloc" -gt 0 ] \
    || fail "DS${n}: file exists but allocates 0 blocks (no real I/O)"
  printf '  DS%d: %s allocated\n' "$n" "$(awk -v b="$ds_alloc" \
    'BEGIN { printf "%.1f MiB", b / 1048576 }')"
done

mds_blocks=$(stat -f %b "$EXPORT_PATH/$VOLUME_FILE" 2>/dev/null || echo 0)
mds_alloc=$(( mds_blocks * 512 ))
printf '  MDS: %d bytes allocated (should be ~0 — metadata-only)\n' "$mds_alloc"
ok "both DSes hold real data"

# ──────────────────────────────────────────────────────────────────────
# (5) DeleteVolume removes the MDS file
# ──────────────────────────────────────────────────────────────────────

step "DeleteVolume($VOLUME_ID)"
"$BIN_DIR/pnfs-csi-cli" delete \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" \
  || fail "DeleteVolume failed"

[ ! -f "$EXPORT_PATH/$VOLUME_FILE" ] \
  || fail "MDS file still present after DeleteVolume: $EXPORT_PATH/$VOLUME_FILE"
ok "MDS file removed"

# ──────────────────────────────────────────────────────────────────────
# (6) Second create after delete — no stale state
# ──────────────────────────────────────────────────────────────────────

step "second CreateVolume (verify no stale state)"
CTX2=$("$BIN_DIR/pnfs-csi-cli" create \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" \
  --size-bytes "$SIZE_BYTES" 2>&1) \
  || { echo "$CTX2"; fail "second CreateVolume failed"; }

EXPORT_PATH2=$(echo "$CTX2" | jq -r '."pnfs.flint.io/export-path"')
[ -f "$EXPORT_PATH2/$VOLUME_FILE" ] \
  || fail "second CreateVolume didn't recreate file"

# Clean up the second volume so we leave the MDS in a sane state.
"$BIN_DIR/pnfs-csi-cli" delete \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" >/dev/null \
  || fail "final cleanup DeleteVolume failed"
ok "create-after-delete works (idempotent provisioner is safe)"

# ──────────────────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────────────────

echo
echo "════════════════════════════════════════════════════════════════"
echo "✓ PASS: pNFS CSI integration end-to-end"
echo "════════════════════════════════════════════════════════════════"
echo "  Create → mount → write → read → unmount → delete → re-create"
echo "  All gRPC verbs and the kernel data path exercised."
echo "  See $LOG_DIR/flint-pnfs-mds.log for the MDS-side trace."
