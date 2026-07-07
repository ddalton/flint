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
# What it asserts (directory-per-volume model)
#   1. CreateVolume returns a volume_context carrying the six
#      pnfs.flint.io/* keys (mds-ip, mds-port, export-path,
#      volume-file, size-bytes, volume-mode) with volume-mode=dir,
#      and the MDS created a directory.
#   2. Mounting the per-volume subtree (MDS:/<volume>) with production
#      mount options succeeds — the NodePublish mount shape.
#   3. Round-trip data integrity (sha256 written == sha256 read back).
#   4. Both DSes have stripe content after the run (real striping —
#      files one level deeper still stripe).
#   5. ISOLATION: a second volume's mount is empty — it sees neither
#      the first volume's files nor its sparse image (the Spark
#      dry-run Finding 1 refutation).
#   6. DeleteVolume removes the whole subtree.
#   7. A re-created volume with the same name mounts EMPTY — no
#      leftover files from the deleted incarnation.
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
VM_MOUNT2="/mnt/flint-csi-e2e-b"
VOLUME_ID="pvc-csi-e2e-test"
VOLUME_ID2="pvc-csi-e2e-other"
SIZE_BYTES=$((512 * 1024 * 1024))   # 512 MiB — small but big enough
                                     # to land multiple stripes on
                                     # both DSes (8 MiB stripe).

# ──────────────────────────────────────────────────────────────────────
# Cleanup
# ──────────────────────────────────────────────────────────────────────

cleanup() {
  set +e
  limactl shell "$LIMA_VM" -- sudo bash -c \
    "umount -lf $VM_MOUNT; umount -lf $VM_MOUNT2" 2>/dev/null
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

# Assert all six keys are present (the contract from the
# pnfs_csi::ctx_keys module — if any of these is missing or renamed,
# this test catches it before the CSI driver does in production).
for key in \
    "pnfs.flint.io/mds-ip" \
    "pnfs.flint.io/mds-port" \
    "pnfs.flint.io/export-path" \
    "pnfs.flint.io/volume-file" \
    "pnfs.flint.io/size-bytes" \
    "pnfs.flint.io/volume-mode"; do
  echo "$CTX_JSON" | jq -e "has(\"$key\")" >/dev/null \
    || fail "volume_context missing required key: $key"
done
ok "volume_context carries all six required keys"

EXPORT_PATH=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/export-path"')
VOLUME_FILE=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/volume-file"')
VOLUME_MODE=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/volume-mode"')
SIZE_REPORTED=$(echo "$CTX_JSON" | jq -r '."pnfs.flint.io/size-bytes"')
[ "$SIZE_REPORTED" = "$SIZE_BYTES" ] \
  || fail "size mismatch: requested $SIZE_BYTES, got $SIZE_REPORTED"
[ "$VOLUME_MODE" = "dir" ] \
  || fail "new volume should be volume-mode=dir, got '$VOLUME_MODE'"

# Verify the MDS actually created a directory.
[ -d "$EXPORT_PATH/$VOLUME_FILE" ] \
  || fail "MDS directory not present at $EXPORT_PATH/$VOLUME_FILE"
ok "MDS-side directory volume present"

# ──────────────────────────────────────────────────────────────────────
# (2) Mount the per-volume subtree — the NodePublish mount shape
# ──────────────────────────────────────────────────────────────────────

step "mount per-volume subtree on Lima client"
limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT"

MOUNT_OPTS="minorversion=1,proto=tcp,port=$MDS_PORT,nconnect=4,rsize=1048576,wsize=1048576,noresvport"
limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "$MOUNT_OPTS" \
  "$HOST_ADDR:/$VOLUME_ID" "$VM_MOUNT" \
  || fail "subtree mount failed (see $LOG_DIR/flint-pnfs-mds.log)"
ok "mounted MDS:/$VOLUME_ID with production options"

# A fresh volume must be empty — the export root (other volumes, stale
# files) must NOT be visible.
ENTRIES=$(limactl shell "$LIMA_VM" -- bash -c "ls -A $VM_MOUNT | wc -l")
[ "$(echo "$ENTRIES" | tr -d '[:space:]')" = "0" ] \
  || { limactl shell "$LIMA_VM" -- ls -al "$VM_MOUNT"; fail "fresh volume is not empty ($ENTRIES entries)"; }
ok "fresh volume mounts empty"

# ──────────────────────────────────────────────────────────────────────
# (3) Round-trip data integrity (file inside the volume subtree)
# ──────────────────────────────────────────────────────────────────────

step "write + sha256 round-trip"
WRITE_HASH=$(limactl shell "$LIMA_VM" -- bash -c "
  set -eu
  dd if=/dev/urandom of=$VM_MOUNT/data.bin bs=1M count=64 conv=notrunc \
    2>/dev/null
  sync
  sha256sum $VM_MOUNT/data.bin | awk '{print \$1}'
")
[ -n "$WRITE_HASH" ] || fail "write hash empty"
ok "wrote 64 MiB, sha256=${WRITE_HASH:0:16}…"

# Drop client cache so the read actually goes through pNFS.
limactl shell "$LIMA_VM" -- sudo bash -c 'sync && echo 3 > /proc/sys/vm/drop_caches' \
  >/dev/null 2>&1

READ_HASH=$(limactl shell "$LIMA_VM" -- bash -c \
  "sha256sum $VM_MOUNT/data.bin | awk '{print \$1}'")
[ "$READ_HASH" = "$WRITE_HASH" ] \
  || fail "data corruption: wrote $WRITE_HASH, read back $READ_HASH"
ok "round-trip hash matches"

# ──────────────────────────────────────────────────────────────────────
# (3b) Long filenames — id-based (v2) filehandles
#      Spark part names used to blow the v1 handle's ~85-byte path
#      budget → EIO on OPEN + un-deletable stripe debris (dry-run
#      Finding 4). Full lifecycle must work now.
# ──────────────────────────────────────────────────────────────────────

step "long-filename lifecycle (v2 filehandle)"
LONG_NAME="part-00000-a1b2c3d4-e5f6-7890-abcd-ef0123456789-c000.snappy.parquet.$(printf 'x%.0s' $(seq 1 60))"
LONG_HASH=$(limactl shell "$LIMA_VM" -- bash -c "
  set -eu
  mkdir -p $VM_MOUNT/spark-output/_temporary/attempt_0
  dd if=/dev/urandom of=$VM_MOUNT/spark-output/_temporary/attempt_0/$LONG_NAME \
    bs=1M count=8 conv=notrunc 2>/dev/null
  sync
  # commit-by-rename, the committer's shape
  mv $VM_MOUNT/spark-output/_temporary/attempt_0/$LONG_NAME $VM_MOUNT/spark-output/$LONG_NAME
  sha256sum $VM_MOUNT/spark-output/$LONG_NAME | awk '{print \$1}'
") || fail "long-filename write/rename failed (v1 path-length limit regressed?)"
[ -n "$LONG_HASH" ] || fail "long-filename hash empty"

LONG_HASH2=$(limactl shell "$LIMA_VM" -- bash -c \
  "sha256sum $VM_MOUNT/spark-output/$LONG_NAME | awk '{print \$1}'")
[ "$LONG_HASH2" = "$LONG_HASH" ] || fail "long-filename data mismatch after rename"

limactl shell "$LIMA_VM" -- bash -c "
  set -eu
  rm $VM_MOUNT/spark-output/$LONG_NAME
  rm -rf $VM_MOUNT/spark-output
" || fail "long-filename delete failed (un-deletable debris — Finding 4 shape)"
ok "long name: write → rename-commit → read → delete all clean"

# ──────────────────────────────────────────────────────────────────────
# (4) Per-DS striping evidence (files one level deeper still stripe)
# ──────────────────────────────────────────────────────────────────────

step "verifying striping"
# Each DS should hold stripe content. What matters is that BOTH DSes
# have actual block allocations; a fall-back-to-MDS-direct path would
# leave one or both DSes empty.
# macOS BSD `stat -f %b` returns blocks-allocated in 512-byte units
# (matches the kernel's st_blocks). Multiplying by 512 gives bytes
# actually on disk for sparse files. Don't use `%B` — that's the
# *filesystem* block size, unrelated to per-file allocation.
for n in 1 2; do
  ds_dir="/tmp/flint-pnfs-ds${n}"
  # Since the P0-2 identity work, DS stripe files are identity-keyed
  # ({file_id:016x}.stripeN), never path-named — the volume name does
  # not appear on a DS. Any allocated stripe file proves DS-path I/O
  # ran (an MDS-direct fallback would leave the DS empty).
  ds_file=$(find "$ds_dir" -type f -name "*.stripe*" | head -1)
  [ -n "$ds_file" ] || fail "DS${n} has no stripe file for $VOLUME_ID/data.bin"
  ds_blocks=$(stat -f %b "$ds_file")
  ds_alloc=$(( ds_blocks * 512 ))
  [ "$ds_alloc" -gt 0 ] \
    || fail "DS${n}: file exists but allocates 0 blocks (no real I/O)"
  printf '  DS%d: %s allocated\n' "$n" "$(awk -v b="$ds_alloc" \
    'BEGIN { printf "%.1f MiB", b / 1048576 }')"
done

mds_blocks=$(stat -f %b "$EXPORT_PATH/$VOLUME_FILE/data.bin" 2>/dev/null || echo 0)
mds_alloc=$(( mds_blocks * 512 ))
printf '  MDS: %d bytes allocated (should be ~0 — metadata-only)\n' "$mds_alloc"
ok "both DSes hold real data"

# ──────────────────────────────────────────────────────────────────────
# (5) ISOLATION: a second volume must not see the first volume's data
# ──────────────────────────────────────────────────────────────────────

step "isolation: second volume sees nothing of the first"
CTX2_JSON=$("$BIN_DIR/pnfs-csi-cli" create \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID2" \
  --size-bytes "$SIZE_BYTES" 2>&1) || { echo "$CTX2_JSON"; fail "CreateVolume($VOLUME_ID2) failed"; }

limactl shell "$LIMA_VM" -- sudo mkdir -p "$VM_MOUNT2"
limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "$MOUNT_OPTS" \
  "$HOST_ADDR:/$VOLUME_ID2" "$VM_MOUNT2" \
  || fail "mount of second volume failed"

ENTRIES2=$(limactl shell "$LIMA_VM" -- bash -c "ls -A $VM_MOUNT2 | wc -l")
[ "$(echo "$ENTRIES2" | tr -d '[:space:]')" = "0" ] \
  || { limactl shell "$LIMA_VM" -- ls -al "$VM_MOUNT2"; \
       fail "second volume is not isolated ($ENTRIES2 entries visible)"; }

# ..-traversal out of the subtree must not reach volume 1's data.
limactl shell "$LIMA_VM" -- bash -c \
  "cat $VM_MOUNT2/../$VOLUME_ID/data.bin >/dev/null 2>&1" \
  && fail "second volume can path-traverse into the first"

limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT2"
"$BIN_DIR/pnfs-csi-cli" delete \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID2" >/dev/null || fail "cleanup of $VOLUME_ID2 failed"
ok "second volume mounts empty and cannot reach the first"

# ──────────────────────────────────────────────────────────────────────
# (6) DeleteVolume removes the whole subtree
# ──────────────────────────────────────────────────────────────────────

step "DeleteVolume($VOLUME_ID)"
limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT"
"$BIN_DIR/pnfs-csi-cli" delete \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" \
  || fail "DeleteVolume failed"

[ ! -e "$EXPORT_PATH/$VOLUME_FILE" ] \
  || fail "MDS subtree still present after DeleteVolume: $EXPORT_PATH/$VOLUME_FILE"
ok "MDS subtree removed"

# ──────────────────────────────────────────────────────────────────────
# (7) Re-create after delete — fresh and EMPTY (no stale state)
# ──────────────────────────────────────────────────────────────────────

step "re-CreateVolume (verify no stale state)"
CTX3=$("$BIN_DIR/pnfs-csi-cli" create \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" \
  --size-bytes "$SIZE_BYTES" 2>&1) \
  || { echo "$CTX3"; fail "second CreateVolume failed"; }

EXPORT_PATH3=$(echo "$CTX3" | jq -r '."pnfs.flint.io/export-path"')
[ -d "$EXPORT_PATH3/$VOLUME_FILE" ] \
  || fail "re-CreateVolume didn't recreate the directory"

limactl shell "$LIMA_VM" -- sudo mount -t nfs4 \
  -o "$MOUNT_OPTS" \
  "$HOST_ADDR:/$VOLUME_ID" "$VM_MOUNT" \
  || fail "mount of re-created volume failed"
ENTRIES3=$(limactl shell "$LIMA_VM" -- bash -c "ls -A $VM_MOUNT | wc -l")
[ "$(echo "$ENTRIES3" | tr -d '[:space:]')" = "0" ] \
  || fail "re-created volume shows stale files ($ENTRIES3 entries)"
limactl shell "$LIMA_VM" -- sudo umount "$VM_MOUNT"

# Clean up the re-created volume so we leave the MDS in a sane state.
"$BIN_DIR/pnfs-csi-cli" delete \
  --endpoint "127.0.0.1:$MDS_GRPC_PORT" \
  --volume-id "$VOLUME_ID" >/dev/null \
  || fail "final cleanup DeleteVolume failed"
ok "re-created volume is fresh and empty"

# ──────────────────────────────────────────────────────────────────────
# Summary
# ──────────────────────────────────────────────────────────────────────

echo
echo "════════════════════════════════════════════════════════════════"
echo "✓ PASS: pNFS CSI integration end-to-end (directory-per-volume)"
echo "════════════════════════════════════════════════════════════════"
echo "  Create → subtree mount → write → read → isolation → delete →"
echo "  re-create-empty. All gRPC verbs and the kernel data path"
echo "  exercised. See $LOG_DIR/flint-pnfs-mds.log for the MDS trace."
