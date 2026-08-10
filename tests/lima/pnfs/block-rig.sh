#!/usr/bin/env bash
#
# pnfs-block kernel-client rig: a STOCK Linux kernel drives the whole
# RFC 8154/9561 chain — LAYOUTGET(type 5) → extent allocation →
# GETDEVICEINFO (NGUID designator) → raw NVMe/TCP I/O against spdk-tgt
# → LAYOUTCOMMIT → REMOVE-driven reclaim. First real bytes through the
# block layout; design doc §11's phase-2 "prove device resolution +
# data path" milestone.
#
# Topology (everything inside the lima VM — the block export needs a
# colocated spdk-tgt over a unix socket, which cannot exist on macOS):
#
#   spdk_tgt (--no-huge, aio file bdev, lvstore lvs_rig)
#   flint-pnfs-mds (sqlite state, blockExport → the tgt socket)
#   kernel client: mounts 127.0.0.1:/<vol> vers=4.2 (the CSI shape),
#     nvme-tcp session STAGED BY `pnfs-csi-cli stage` — the production
#     csi-node path (AttachBlockNode admission + ensure_session), not a
#     bash reimplementation of it
#
# Prerequisites (see reference_linux_test_crossbuild + this session):
#   - VM kernel ≥ 6.11 with blocklayoutdriver (HWE kernel installed)
#   - ~/rig-spdk/{spdk_tgt,scripts,py,grpcurl} (extracted from the
#     arm64 spdk-tgt image; grpcurl static linux_arm64)
#   - cross-built release MDS + CSI CLI:
#       cargo build --release --target aarch64-unknown-linux-musl \
#         --bin flint-pnfs-mds --bin pnfs-csi-cli   (zig-shim recipe)
#
# PROOFS asserted, in order of strength:
#   1. sha256(cold read via NFS) == sha256(source) — data integrity
#      through client-side raw-extent I/O.
#   2. bdev_get_iostat on the volume's lvol: bytes_written and
#      bytes_read ≥ file size — the DEVICE served the bytes (runbm
#      arbiter lesson: the device counters are the truth; a cache or a
#      fallback path cannot fake them).
#   3. MDS log: LAYOUTGET (scsi) granted ≥ 1, zero zeros-belt refusals
#      (no MDS-path I/O happened at all).
#   4. sqlite: committed (state='rw') extent rows exist after sync —
#      LAYOUTCOMMIT promoted through the allocator.
#   5. REMOVE reclaims: extent rows for the volume drain to 0 (clean
#      free via the client's LAYOUTRETURN, not quarantine).
#
# FENCE=1 — the FenceReaches drill (design §9: the phase-2 rig that
# proves real preempt delivery). Between proofs 4 and 5, against a LIVE
# raw-path writer:
#   F1. pre-fence: two lvol iostat samples GROW — the writer is on the
#       raw NVMe path when the fence lands, not before/after it.
#   F2. FenceBlockClient (the operator lever) answers fenced=true and
#       the MDS log shows the reservation preempt (preempted=true).
#   F3. post-fence: two iostat samples are EQUAL — the bytes STOPPED.
#       This is FenceReaches, on the device counters, against a client
#       whose session was up and mid-write.
#   F4. the writer loop exits nonzero (EIO surfaced to userspace) and a
#       fresh O_DIRECT write cannot move the counters either.
#   F5. the durable arm is recorded at the MDS and the eviction reaches
#       the client (its nvme reconnect is refused); the conforming
#       client returns its layout on the write error (return-after-fence).
#   FENCE mode STOPS here — it does not run the destructive REMOVE
#   reclaim below. Reclaim-after-fence is a distinct property covered by
#   the base rig (clean REMOVE) and the allocator unit tests; forcing it
#   here only silly-renames a file the fenced writer still holds open.
#
# FENCE=1 UNFENCE=1 — the fence is REVERSIBLE (§U), via the real
#   operator flow: fence a mid-write client, REBOOT the node (a fenced
#   in-flight O_DIRECT pwrite parks in D-state — rig-found; no umount,
#   lazy or not, gets past it), let the restarted stack re-establish
#   the fence from the durable record, then UnfenceBlockClient clears
#   the record + RELEASES the EA-RO reservation, the evicted client
#   reconnects, and its O_DIRECT write moves the counter the fence
#   froze.
# FENCE=1 RESTART=1 — the fence survives an MDS restart (§R).
# FENCE=1 TGT_RESTART=1 [PTPL_LOSS=1] — the fence survives a tgt
#   restart via ptpl (or, with the ptpl_file destroyed, via the durable
#   fenced_clients record) (§T).
# RECONCILE=1 — a tgt-ONLY restart repairs WITHOUT an MDS roll (§C):
#   the periodic export reconcile loop rebuilds the export chain from
#   sqlite (MDS pid asserted unchanged), the client's surviving kernel
#   controller reconnects, raw bytes flow again, and the run falls
#   through to REMOVE reclaim + unstage/detach on the repaired stack.
#
# Exit 0 = every proof held.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
RIG_TOOLS="${RIG_TOOLS:-$HOME/rig-spdk}"          # same absolute path inside the VM
MDS_BIN="$REPO_ROOT/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-pnfs-mds"
CSI_CLI="$REPO_ROOT/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/pnfs-csi-cli"
CFG="$REPO_ROOT/tests/lima/pnfs/mds-block.yaml"
PROTO_DIR="$REPO_ROOT/spdk-csi-driver/proto"

VOL=rigvol
VOL_BYTES=$((512 * 1024 * 1024))
IO_MIB=64
# ctrl-loss for staged sessions: the FENCE drills want it LOW (the
# D-state safety belt), but the RECONCILE drill needs the kernel
# controller to SURVIVE the tgt restart whose repair it proves — the
# production default is 1800s, so a long value IS the production shape.
CTRL_LOSS=10
[ "${RECONCILE:-0}" = "1" ] && CTRL_LOSS=120
# The periodic export reconcile loop, tightened for rig timescales
# (production default 30s). Set on every MDS launch so all modes run
# the production loop; only RECONCILE=1 depends on it.
RECON_SECS=5
SUBNQN="nqn.2024-11.com.flint:block:$VOL"
SOCK=/var/tmp/spdk-rig.sock
RIG=/var/tmp/flint-rig
MNT=/mnt/flint-block

RPC="sudo PYTHONPATH=$RIG_TOOLS/py python3 $RIG_TOOLS/scripts/rpc.py -s $SOCK"

vsh()  { limactl shell "$LIMA_VM" -- bash -c "$*"; }
vsudo(){ limactl shell "$LIMA_VM" -- sudo bash -c "$*"; }

fail() {
  echo "✗ $*"
  echo "── MDS log tail ──"; vsh "tail -30 $RIG/mds.log 2>/dev/null" || true
  echo "── client dmesg (nfs/pnfs/nvme) ──"
  vsudo "dmesg | grep -iE 'nfs|pnfs|blocklayout|nvme' | tail -20" || true
  exit 1
}

cleanup() {
  set +e
  # The FENCE=1 continuous writer FIRST: a leftover dd-loop from a
  # failed fence run keeps writing $MNT/data.bin, and the next run's
  # step-8 urandom write races it → a phantom sha mismatch that looks
  # like data corruption but is just a ghost from the last run.
  vsudo "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null;
         pkill -9 -f 'of=$MNT/data.bin'; pkill -9 -f rig-writer.py;
         rm -f /var/tmp/rig-writer.pid /var/tmp/rig-writer.done /var/tmp/rig-writer.err" \
    >/dev/null 2>&1
  vsudo "umount -lf $MNT 2>/dev/null; nvme disconnect -n $SUBNQN 2>/dev/null" >/dev/null 2>&1
  # Kill AND wait: a half-dead spdk_tgt still holds the core-claim lock
  # AND the 4420 listener, and a successor dies on either. Two traps
  # already paid for here: (1) -x exact comm, NEVER -f — the -f pattern
  # matches the sudo/bash wrapper's own cmdline and pkill kills its own
  # shell mid-sweep; (2) a RUNNING spdk_tgt's comm is `reactor_0` (SPDK
  # renames its main thread), so `pkill -x spdk_tgt` only ever matches
  # one that hasn't finished booting.
  vsudo "[ -f /var/tmp/spdk-rig.pid ] && kill -9 \$(cat /var/tmp/spdk-rig.pid) 2>/dev/null;
         pkill -9 -x reactor_0; pkill -9 -x spdk_tgt; pkill -9 -x flint-pnfs-mds;
         rm -f /var/tmp/spdk-rig.pid;
         for i in \$(seq 1 20); do
           { pgrep -x reactor_0 || pgrep -x spdk_tgt; } >/dev/null || exit 0; sleep 0.5;
         done" \
    >/dev/null 2>&1
}
# KEEP=1 leaves the whole stack running for post-mortem inspection.
[ "${KEEP:-0}" = "1" ] || trap cleanup EXIT

# ── 0. preflight ──────────────────────────────────────────────────────
[ -x "$MDS_BIN" ] || { echo "✗ missing $MDS_BIN — cross-build it first (see header)"; exit 1; }
[ -x "$CSI_CLI" ] || { echo "✗ missing $CSI_CLI — cross-build it first (see header)"; exit 1; }
KREL=$(vsh "uname -r")
KMAJ=${KREL%%.*}; KMIN=$(echo "$KREL" | cut -d. -f2)
if [ "$KMAJ" -lt 6 ] || { [ "$KMAJ" -eq 6 ] && [ "$KMIN" -lt 11 ]; }; then
  echo "✗ VM kernel $KREL < 6.11 — a below-floor client SILENTLY degrades to MDS I/O (§4a)"
  exit 1
fi
vsh "test -f /lib/modules/$KREL/kernel/fs/nfs/blocklayout/blocklayoutdriver.ko*" \
  || { echo "✗ blocklayoutdriver module missing for $KREL"; exit 1; }
vsh "test -x $RIG_TOOLS/spdk_tgt && test -x $RIG_TOOLS/grpcurl" \
  || { echo "✗ $RIG_TOOLS incomplete (spdk_tgt/grpcurl)"; exit 1; }
echo "▶ block-rig on $LIMA_VM (kernel $KREL)"

# ── 1. clean slate ────────────────────────────────────────────────────
cleanup
vsh "pgrep -x spdk_tgt || pgrep -x reactor_0" >/dev/null && fail "an spdk_tgt survived cleanup — refusing to start a second writer over the same aio file"
# Stale SIGKILL residue: the cpu-lock file outlives a killed tgt long
# enough to make the successor lose its core claim and die. And stale
# nvme-eui by-id links from a previous run dangle once the device is
# gone — a dangling link reads as "resolution works" right up until the
# kernel opens it (observed: a stale link made udev look §4a-clean).
vsudo "rm -rf $RIG /var/tmp/rig-disk.img $SOCK ${SOCK}.lock /var/tmp/spdk_cpu_lock_*;
       rm -f /dev/disk/by-id/nvme-eui.*;
       mkdir -p $RIG/exports $MNT; chmod 0777 $RIG $RIG/exports"

# ── 2. spdk_tgt + lvstore + transport ────────────────────────────────
# --wait-for-rpc + minimized pools is the kind-tier recipe (default
# pools want ~1.4GB and the VM has 2GB total; minimized ≈ 26MB).
vsudo "truncate -s 1G /var/tmp/rig-disk.img"
vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $SOCK -m 0x1 --wait-for-rpc >$RIG/spdk.log 2>&1 &
       echo \$! > /var/tmp/spdk-rig.pid; sleep 0.5"
for i in $(seq 1 20); do
  vsh "$RPC rpc_get_methods >/dev/null 2>&1" && break
  [ "$i" = 20 ] && fail "spdk_tgt RPC never came up ($RIG/spdk.log)"
  sleep 0.5
done
# The MDS runs unprivileged; the root-owned rpc socket must admit it.
vsudo "chmod 0777 $SOCK"
vsh "$RPC iobuf_set_options --small-pool-count 4096 --large-pool-count 1024" \
  || fail "iobuf_set_options"
vsh "$RPC iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1" \
  || fail "iscsi_set_options"
vsh "$RPC framework_start_init" || fail "framework_start_init"
for i in $(seq 1 60); do
  vsh "$RPC framework_wait_init >/dev/null 2>&1" && break
  [ "$i" = 60 ] && fail "SPDK subsystems never initialized ($RIG/spdk.log)"
  sleep 0.5
done
vsh "$RPC bdev_aio_create /var/tmp/rig-disk.img rigdisk 4096" >/dev/null || fail "bdev_aio_create"
vsh "$RPC bdev_lvol_create_lvstore rigdisk lvs_rig" >/dev/null || fail "create_lvstore"
vsh "$RPC nvmf_create_transport -t TCP" || fail "nvmf_create_transport"
echo "✓ spdk_tgt up: aio(1G) → lvs_rig, TCP transport"

# ── 3. MDS ────────────────────────────────────────────────────────────
MDS_LOG="${MDS_LOG:-info}"
vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS RUST_LOG='$MDS_LOG' nohup $MDS_BIN --config $CFG >$RIG/mds.log 2>&1 & sleep 0.5"
for i in $(seq 1 20); do
  vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
  [ "$i" = 20 ] && fail "MDS gRPC never came up"
  sleep 0.5
done
echo "✓ MDS up (nfs 20490, grpc 50051)"

# ── 4. CreateVolume (scsi class) — arena + lvol + subsystem + NGUID ──
CV=$(vsh "$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
      -d '{\"volumeId\":\"$VOL\",\"sizeBytes\":$VOL_BYTES,\"layoutClass\":\"scsi\"}' \
      127.0.0.1:50051 pnfs.control.MdsControl/CreateVolume") || fail "CreateVolume RPC"
echo "$CV" | grep -c '"created": true' >/dev/null || fail "CreateVolume refused: $CV"
echo "$CV" | grep -c '"effectiveLayoutClass": "scsi"' >/dev/null || fail "class echo wrong: $CV"
SUBS=$(vsh "$RPC nvmf_get_subsystems")
echo "$SUBS" | grep -c "$SUBNQN" >/dev/null || fail "subsystem $SUBNQN not created"
NGUID=$(echo "$SUBS" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(s['namespaces'][0]['nguid'].lower()); break
")
[ -n "$NGUID" ] || fail "no namespace/NGUID on $SUBNQN"
echo "✓ volume created: class=scsi, subsystem live, NGUID=$NGUID"

# ── 5. stage the node session — THE PRODUCTION PATH ──────────────────
# AttachBlockNode (per-node hostnqn admission, the ControllerPublish
# verb — the allow-list is default-closed, so this must precede the
# connect) + pnfs_block_session::ensure_session (connect as the
# MDS-admitted NQN, fast_io_fail sysfs backfill, §4a eui link,
# NGUID-matched head-device resolution). The same code csi-node runs at
# NodeStage, driven through pnfs-csi-cli so the rig proves shipped code
# instead of a bash reimplementation of it. Low ctrl-loss / fast-io-fail
# stays the §6 mandate AND this drill's D-state safety belt.
HOSTNQN="nqn.2024-11.com.flint:node:$(vsh hostname)"
vsudo "modprobe nvme-tcp && modprobe blocklayoutdriver"
STAGE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
        $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
  || fail "pnfs-csi-cli stage (attach or session)"
NSDEV=$(basename "$(echo "$STAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)")
[ -n "$NSDEV" ] || fail "stage reported no device: $STAGE"
# Three coordinate assertions close the drift loops: the MDS-admitted
# NQN must equal the co_ownerid derivation LAYOUTGET admission uses;
# the attach-answered NGUID must equal what the tgt's namespace really
# carries (section 4's truth); and fast_io_fail must have LANDED, not
# merely been attempted (a rejected sysfs write is silent).
echo "$STAGE" | grep -c "\"hostNqn\":\"$HOSTNQN\"" >/dev/null \
  || fail "MDS-admitted NQN diverges from the co_ownerid derivation: $STAGE"
STAGE_NGUID=$(echo "$STAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["nguid"])' 2>/dev/null)
[ "$STAGE_NGUID" = "$NGUID" ] || fail "attach NGUID '$STAGE_NGUID' != target namespace NGUID '$NGUID'"
FIF=$(vsh "cat /sys/class/nvme/*/fast_io_fail_tmo 2>/dev/null | head -1 | tr -d ' '" || true)
[ "${FIF:-}" = "5" ] || fail "fast_io_fail_tmo not applied by ensure_session (got '${FIF:-none}')"
echo "✓ staged (production path): /dev/$NSDEV as $HOSTNQN, fast_io_fail=5s"

# ── 6. the §4a udev landmine — observe, then prove the repair ────────
# ensure_session already closed it. To OBSERVE the landmine we remove
# the link, replay the udev add event, and check what udev creates on
# its own; then a second stage — idempotent: the live controller is
# reused, no second connect — must repair the link. That re-stage IS
# the production shape of every kubelet NodeStage retry.
vsudo "rm -f /dev/disk/by-id/nvme-eui.$NGUID"
vsudo "udevadm trigger --action=add /dev/$NSDEV 2>/dev/null; udevadm settle -t 10" || true
NATIVE=$(vsh "ls /dev/disk/by-id/ 2>/dev/null | grep -c '^nvme-eui\.$NGUID\$'" || true)
if [ "${NATIVE:-0}" = "0" ]; then
  echo "· udev did NOT create nvme-eui.$NGUID (§4a landmine CONFIRMED on $KREL)"
else
  echo "· udev created nvme-eui.$NGUID natively (§4a landmine ABSENT on $KREL — update the doc)"
fi

# The PACKAGED rule (the file the driver binary embeds and installs on
# every node when pnfs-block is enabled): with it installed, udev must
# create the link NATIVELY — the property the stage-time managed link
# cannot provide (device re-adds while staged re-link with no flint
# involvement). Then remove it, so the VM stays pristine and the
# managed-link repair below is tested against a bare udev.
RULE_SRC="$REPO_ROOT/spdk-csi-driver/files/99-flint-pnfs-eui.rules"
[ -f "$RULE_SRC" ] || fail "packaged udev rule missing at $RULE_SRC"
vsudo "cp $RULE_SRC /etc/udev/rules.d/99-flint-pnfs-eui.rules; udevadm control --reload
       udevadm trigger --action=add /dev/$NSDEV; udevadm settle -t 10"
RLINK=$(vsh "readlink /dev/disk/by-id/nvme-eui.$NGUID" || true)
echo "$RLINK" | grep -c "$NSDEV" >/dev/null \
  || fail "the PACKAGED udev rule did not create nvme-eui.$NGUID (readlink: '${RLINK:-none}')"
vsudo "rm -f /etc/udev/rules.d/99-flint-pnfs-eui.rules /dev/disk/by-id/nvme-eui.$NGUID
       udevadm control --reload"
echo "✓ packaged udev rule creates the eui link natively (then removed — VM stays pristine)"
vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
       $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)" >/dev/null \
  || fail "re-stage (link repair) failed"
LINK=$(vsh "readlink /dev/disk/by-id/nvme-eui.$NGUID" || true)
echo "$LINK" | grep -c "$NSDEV" >/dev/null \
  || fail "re-stage did not repair the eui link (readlink: '${LINK:-none}')"
echo "✓ stage is idempotent and re-links (§4a repair proven through production code)"

# ── 7. mount the VOLUME SUBDIR (the CSI shape — fsinfo must read the
#       scsi class, not the pseudo-root's files advertisement) ────────
vsudo "mount -t nfs4 -o vers=4.2,proto=tcp,port=20490 127.0.0.1:/$VOL $MNT" || fail "mount"
echo "✓ mounted 127.0.0.1:/$VOL (vers=4.2)"

# ── 8. write → sync → cold read → verify ─────────────────────────────
vsh "dd if=/dev/urandom of=/var/tmp/rig-src.bin bs=1M count=$IO_MIB status=none"
SRC_SHA=$(vsh "sha256sum /var/tmp/rig-src.bin" | awk '{print $1}')
vsudo "cp /var/tmp/rig-src.bin $MNT/data.bin && sync $MNT/data.bin" || fail "write through the mount"
# LAYOUTCOMMIT must land before the size/extents are believable.
COMMITTED=0
for i in $(seq 1 20); do
  COMMITTED=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM extents WHERE volume='$VOL' AND state='rw'\"" || echo 0)
  [ "${COMMITTED:-0}" -ge 1 ] && break
  sleep 0.5
done
[ "${COMMITTED:-0}" -ge 1 ] || fail "no committed (rw) extents after sync — LAYOUTCOMMIT never landed"
vsudo "echo 3 > /proc/sys/vm/drop_caches"
DST_SHA=$(vsudo "sha256sum $MNT/data.bin" | awk '{print $1}')
[ "$SRC_SHA" = "$DST_SHA" ] || fail "sha mismatch: src=$SRC_SHA dst=$DST_SHA"
echo "✓ ${IO_MIB}MiB wrote+cold-read intact (sha256 $SRC_SHA), $COMMITTED extent row(s) committed"

# ── 9. the device counters are the truth (runbm arbiter) ─────────────
read -r WR RD <<<"$(vsh "$RPC bdev_get_iostat --name lvs_rig/$VOL" | python3 -c "
import json,sys
b = json.load(sys.stdin)['bdevs'][0]
print(b['bytes_written'], b['bytes_read'])
")"
NEED=$((IO_MIB * 1024 * 1024))
[ "${WR:-0}" -ge "$NEED" ] || fail "lvol saw only $WR bytes written (< $NEED) — the data path did NOT go through NVMe"
[ "${RD:-0}" -ge "$NEED" ] || fail "lvol saw only $RD bytes read (< $NEED) — the cold read did NOT come from the device"
GRANTS=$(vsh "grep -c 'LAYOUTGET (scsi) granted' $RIG/mds.log" || true)
BELT=$(vsh "grep -c 'MDS I/O on scsi-class file' $RIG/mds.log" || true)
[ "${GRANTS:-0}" -ge 1 ] || fail "MDS log shows no scsi LAYOUTGET grants"
[ "${BELT:-0}" = "0" ] || fail "zeros-belt fired $BELT time(s) — the client fell back to MDS I/O"
echo "✓ device counters: ${WR}B written / ${RD}B read; $GRANTS LAYOUTGET grant(s); zero MDS-path I/O"

lvol_written() {
  vsh "$RPC bdev_get_iostat --name lvs_rig/$VOL" | python3 -c "
import json,sys; print(json.load(sys.stdin)['bdevs'][0]['bytes_written'])"
}

# ── F. FENCE=1: the FenceReaches drill ───────────────────────────────
if [ "${FENCE:-0}" = "1" ]; then
  # F0. a HELD-OPEN O_DIRECT re-writer (rig-writer.py — see its header).
  # The held fd is the point: it keeps the layout (and its grant row)
  # live so there is something to fence, where a per-write dd-loop would
  # LAYOUTRETURN between passes.
  vsudo "rm -f /var/tmp/rig-writer.done
         nohup python3 $REPO_ROOT/tests/lima/pnfs/rig-writer.py \
           $MNT/data.bin /var/tmp/rig-writer.done >/var/tmp/rig-writer.err 2>&1 &
         echo \$! > /var/tmp/rig-writer.pid"

  # F1. the writer is ON the raw path: the device counter grows.
  W1=$(lvol_written); sleep 3; W2=$(lvol_written)
  [ "${W2:-0}" -gt "${W1:-0}" ] \
    || fail "writer never reached the device (bytes_written $W1 → $W2) — nothing to fence"
  echo "✓ live raw-path writer: bytes_written $W1 → $W2 and climbing"

  # The client's own view of the reservation table BEFORE the fence —
  # independent of anything flint logs. Shows whether the kernel even
  # registered a preemptable key (the 'zeroed new key' question).
  RESV_BEFORE=$(vsudo "nvme resv-report /dev/$NSDEV -c 1 -e 2>/dev/null | grep -iE 'rtype|regctl|rkey' | tr '\n' ' '" || true)
  echo "· resv-report pre-fence: ${RESV_BEFORE:-<none>}"

  # F2. the lever. The victim client id is the NFSv4 client id — the
  # same u64 GETDEVICEINFO handed out as the reservation key. It is
  # stable, but a grant ROW only exists mid-write (LAYOUTRETURN drops it
  # between the O_DIRECT writer's passes), so read it from the MDS log's
  # scsi LAYOUTGET/RETURN lines, which always name it, and fall back to
  # a live grant row.
  CID=$(vsh "grep -oE 'client [0-9]+' $RIG/mds.log | grep -oE '[0-9]+' | tail -1" || true)
  [ -n "$CID" ] || CID=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT DISTINCT client_id FROM extent_grants WHERE volume='$VOL' LIMIT 1\"")
  [ -n "$CID" ] || fail "no scsi client id in the MDS log or grant table to fence"
  FR=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/FenceBlockClient") \
    || fail "FenceBlockClient RPC (timed out or refused) — MDS fence breadcrumbs: $(vsh "grep -iE 'fence_preempt|resv fence' $RIG/mds.log | tail -3")"
  echo "$FR" | grep -c '"fenced": true' >/dev/null || fail "fence lever refused: $FR"
  # The fence is EITHER a preempt of the victim's key OR the MDS taking
  # EA-RO with the victim a non-registrant — both leave the client
  # unable to write. Require that the MDS became the EA-RO holder; the
  # device-counter freeze below is the real proof of reach.
  RESV_LINE=$(vsh "grep -o 'resv: .*' $RIG/mds.log | tail -1" || true)
  echo "$RESV_LINE" | grep -q 'rtype=0x4' \
    || fail "MDS did not take an EA-RO reservation — resv: ${RESV_LINE:-<none>}"
  echo "✓ fence lever accepted (client $CID): ${RESV_LINE}"

  # F3. FenceReaches, on the device counters: the bytes STOP. First
  # sample after a settle so in-flight completions drain.
  sleep 3
  W3=$(lvol_written); sleep 3; W4=$(lvol_written)
  [ "${W3:-0}" = "${W4:-1}" ] \
    || fail "bytes_written STILL CLIMBING after the fence ($W3 → $W4) — FenceReaches is FALSE"
  echo "✓ FenceReaches: bytes_written frozen at $W4 across 3s (was climbing pre-fence)"

  # F4. the writer makes no further progress. Two lawful post-fence
  # states, BOTH acceptable — a reservation conflict is a command-level
  # error, not a path loss, so the pNFS client may (a) surface it to the
  # pwrite as an errno, or (b) block retrying it through the MDS
  # fallback (which refuses). The HARD proof is neither errno nor
  # timing: it is that the device counter, frozen in F3, stays frozen —
  # a still-blocked writer is as fenced as an errored one. We give the
  # errno path ~20s to appear (informational) and then re-confirm the
  # freeze.
  DONE=""
  for i in $(seq 1 40); do
    DONE=$(vsh "cat /var/tmp/rig-writer.done 2>/dev/null" || true)
    [ -n "$DONE" ] && break
    sleep 0.5
  done
  W5=$(lvol_written)
  [ "${W5:-1}" = "$W4" ] || fail "the writer resumed after the fence ($W4 → $W5) — FenceReaches is FALSE"
  RESV_DMESG=$(vsudo "dmesg | grep -ci 'reservation conflict'" || true)
  if echo "$DONE" | grep -qE 'EXIT [1-9]'; then
    echo "✓ writer errored out ($DONE); counter still frozen at $W5 (dmesg resv-conflict: ${RESV_DMESG:-0})"
  else
    echo "✓ writer made no progress (blocked on MDS-fallback retry, marker='${DONE:-none}'); counter frozen at $W5 (dmesg resv-conflict: ${RESV_DMESG:-0})"
  fi

  # F5. the durable + functional arms, asserted where they actually show
  # up. A CONFORMING client returns its layout on the write error (dmesg
  # `_pnfs_return_layout`), which cleanly FREES the grant rows the
  # durable arm just marked fenced=1 — the return-after-fence upgrade —
  # so persistent fenced rows are the WRONG artifact to check. Assert
  # instead: (a) the MDS recorded the durable fence, and (b) the
  # eviction reached the client — its nvme-tcp path can no longer
  # reconnect (the allow-list refuses its hostnqn). Together these are
  # the fence being durable at the MDS and enforced at the client.
  vsh "grep -q 'fenced (durable' $RIG/mds.log" \
    || fail "MDS never recorded the durable fence"
  RECONN=$(vsudo "dmesg | grep -c 'is not allowed, hostnqn'" || true)
  [ "${RECONN:-0}" -ge 1 ] \
    || fail "the client's nvme reconnect was not refused — the host eviction did not reach it"
  RET=$(vsudo "dmesg | grep -c '_pnfs_return_layout'" || true)
  echo "✓ durable+functional fence: MDS recorded it; client reconnect refused (${RECONN}×); client returned its layout on error (${RET} dmesg frames)"

  # F6. the ADMISSION side door is closed too: AttachBlockNode (the
  # ControllerPublish verb) for the fenced node is refused at the MDS —
  # and the fence must have swept the node-attach row stage created, or
  # the NQN would still be on the allow-list despite the eviction.
  # Without the NQN-level guard, a rescheduled pod on the SAME node
  # would re-admit the NQN the fence just removed.
  ATT=$(vsh "$CSI_CLI attach --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname) 2>&1; true")
  echo "$ATT" | grep -ci 'fenced' >/dev/null \
    || fail "attach of a fenced node was NOT refused: $ATT"
  NAROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"" || echo "?")
  [ "${NAROWS:-1}" = "0" ] \
    || fail "the fence left $NAROWS node-attach row(s) — the fenced NQN would stay admitted"
  echo "✓ fenced node's AttachBlockNode refused; its node-attach row swept by the fence"

  # The fence is proven. Reap the (fenced) writer and STOP here — the
  # FENCE drill deliberately does NOT go on to the destructive REMOVE
  # reclaim below. Reclaim-after-fence is a distinct property, already
  # covered by the base rig (clean REMOVE) and the allocator unit tests
  # (`scsi_reclaim_fences_the_unresponsive_and_frees_the_returned`,
  # quarantine + clean-free). Forcing it here means `rm` on a file the
  # fenced writer still holds open, which NFS silly-renames rather than
  # unlinks (the MDS re-keys the recall handle, correctly) — proving
  # nothing about reclaim and inviting the ctrl-loss D-state teardown on
  # a memory-tight VM. Keeping the two drills separate is the fix.
  vsudo "nvme disconnect -n $SUBNQN 2>/dev/null; true"
  vsudo "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null; true"

  # ── U. UNFENCE=1: the fence is REVERSIBLE ───────────────────────────
  # The inverse of the whole F-drill, run the way an operator actually
  # runs it: fence the wedged node, REBOOT it, then unfence. The reboot
  # is not rig convenience — it is the only reliable client recovery:
  # an O_DIRECT writer fenced MID-WRITE can park its pwrite in D-state
  # client-side (rig-found: the F4 "blocked" branch left a pwrite even
  # `umount -lf` wedged behind, with nvme-delete-wq in D). Since the
  # whole stack lives in the VM, the reboot doubles as a together-
  # restart of tgt+MDS — composing the ALREADY-PROVEN T-mode property
  # (the fence re-establishes from ptpl + the durable record) in front
  # of the release under test. The money proof inverts F3: the device
  # counter moves again under a fresh O_DIRECT write.
  if [ "${UNFENCE:-0}" = "1" ]; then
    echo "▶ UNFENCE: node reboot, fence re-established, then record → reservation → bytes"
    # U0. reboot the client node (the whole VM — force: the D-state
    # writer blocks a graceful shutdown indefinitely).
    limactl stop -f "$LIMA_VM" || fail "limactl stop"
    limactl start "$LIMA_VM" || fail "limactl start"
    vsudo "modprobe nvme-tcp && modprobe blocklayoutdriver" || fail "modprobe after reboot"
    vsudo "rm -f /var/tmp/spdk_cpu_lock_*; rm -f /dev/disk/by-id/nvme-eui.*"

    # Same tgt resurrection as §T: SAME disk image, SAME ptpl_dir —
    # lvstore+lvol auto-load; NO create_lvstore (that would wipe).
    vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $SOCK -m 0x1 --wait-for-rpc >>$RIG/spdk.log 2>&1 &
           echo \$! > /var/tmp/spdk-rig.pid; sleep 0.5"
    for i in $(seq 1 20); do
      vsh "$RPC rpc_get_methods >/dev/null 2>&1" && break
      [ "$i" = 20 ] && fail "tgt RPC never came back after the reboot ($RIG/spdk.log)"
      sleep 0.5
    done
    vsudo "chmod 0777 $SOCK"
    vsh "$RPC iobuf_set_options --small-pool-count 4096 --large-pool-count 1024" || fail "iobuf (reboot)"
    vsh "$RPC iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1" || fail "iscsi (reboot)"
    vsh "$RPC framework_start_init" || fail "framework_start_init (reboot)"
    for i in $(seq 1 60); do
      vsh "$RPC framework_wait_init >/dev/null 2>&1" && break
      [ "$i" = 60 ] && fail "subsystems never initialized (reboot)"
      sleep 0.5
    done
    vsh "$RPC bdev_aio_create /var/tmp/rig-disk.img rigdisk 4096" >/dev/null || fail "bdev_aio_create (reboot)"
    for i in $(seq 1 40); do
      vsh "$RPC bdev_get_bdevs --name lvs_rig/$VOL >/dev/null 2>&1" && break
      [ "$i" = 40 ] && fail "the lvstore/lvol did NOT auto-load after the reboot"
      sleep 0.5
    done
    vsh "$RPC nvmf_create_transport -t TCP" || fail "nvmf_create_transport (reboot)"

    # MDS back; its startup replay must RE-ESTABLISH the fence from the
    # durable record before the lever lifts it (the §T-proven property,
    # here as a precondition: what the release releases is real).
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
    for i in $(seq 1 20); do
      vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
      [ "$i" = 20 ] && fail "MDS gRPC never came back after the reboot"
      sleep 0.5
    done
    STARTUP=""
    for i in $(seq 1 60); do
      STARTUP=$(vsh "grep 'startup re-fence' $RIG/mds.log | tail -1" || true)
      [ -n "$STARTUP" ] && break
      sleep 0.5
    done
    [ -n "$STARTUP" ] || fail "no startup re-fence after the reboot — the durable record was not consulted"
    echo "$STARTUP" | grep -q '0x666c696e745f6d64(holder)' \
      || fail "the fence did not re-establish across the reboot: ${STARTUP}"
    echo "✓ node rebooted; stack restarted; fence re-established: ${STARTUP}"

    # U1. the lever: record cleared AND the reservation released.
    UR=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
          -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID\"}' \
          127.0.0.1:50051 pnfs.control.MdsControl/UnfenceBlockClient") \
      || fail "UnfenceBlockClient RPC failed"
    echo "$UR" | grep -c '"unfenced": true' >/dev/null || fail "unfence lever refused: $UR"
    REL_LINE=$(vsh "grep -o 'released=.*' $RIG/mds.log | tail -1" || true)
    echo "$REL_LINE" | grep -q 'released=true' \
      || fail "the reservation was NOT released — ${REL_LINE:-<no release line>}"
    echo "✓ unfence lever accepted (client $CID): ${REL_LINE}"

    # U2. the durable record is gone (and stays gone — nothing for a
    # future startup replay to re-fence from).
    FCOUNT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
      \"SELECT COUNT(*) FROM fenced_clients WHERE volume='$VOL'\"" || echo "?")
    [ "${FCOUNT:-1}" = "0" ] || fail "fenced_clients still holds $FCOUNT row(s) after unfence"
    echo "✓ durable record cleared (fenced_clients drained)"

    # U3. the transport path back — through the PRODUCTION path:
    # `pnfs-csi-cli stage` = AttachBlockNode (refused at F6 while the
    # fence stood; it must succeed now that U1 cleared it) +
    # ensure_session (connect as the admitted NQN, fast_io_fail
    # backfill, §4a link — all from scratch on this fresh-boot kernel).
    # At F5 / R4 the connect inside this exact path was refused.
    STAGE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
            $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
      || fail "the unfenced client could not re-stage (attach or connect refused)"
    NSDEV=$(basename "$(echo "$STAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)")
    [ -n "$NSDEV" ] || fail "re-stage reported no device: $STAGE"
    echo "✓ unfenced client re-staged (production path): /dev/$NSDEV (was refused while fenced)"

    # U4. THE INVERSE OF F3: mount, write O_DIRECT, and the device
    # counter moves again. The RELEASE is what this requires: the
    # rebooted client is a fresh NFSv4 identity (new clientid — the
    # admission guard is a same-incarnation belt, unit-tested, not
    # provable here), but no clientid change can dodge a held EA-RO —
    # the reservation blocks by NVMe host registration, and this host
    # registers no key. Only the release lets the raw write through. A
    # fallback-path write cannot fake it — the scsi zeros-belt refuses
    # MDS I/O, so dd would EIO instead of moving the lvol counter
    # (which the tgt restart zeroed: everything it counts now is
    # post-release traffic).
    vsudo "mkdir -p $MNT && mount -t nfs4 -o vers=4.2,proto=tcp,port=20490 127.0.0.1:/$VOL $MNT" \
      || fail "post-unfence mount failed"
    REWRITE_MIB=8
    vsudo "dd if=/dev/urandom of=$MNT/data.bin bs=1M count=$REWRITE_MIB \
           oflag=direct conv=notrunc status=none && sync $MNT/data.bin" \
      || fail "post-unfence O_DIRECT write FAILED — the client is still fenced somewhere"
    W_AFTER=$(lvol_written)
    NEED_RW=$((REWRITE_MIB * 1024 * 1024))
    [ "${W_AFTER:-0}" -ge "$NEED_RW" ] \
      || fail "device saw only ${W_AFTER}B written after unfence (need ≥$NEED_RW) — the write did not go raw"
    HOSTROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
      \"SELECT COUNT(*) FROM block_hosts WHERE volume='$VOL'\"" || echo 0)
    [ "${HOSTROWS:-0}" -ge 1 ] \
      || fail "no block_hosts row after the recovery write — the LAYOUTGET admission never ran"
    echo "✓ bytes flow again: ${W_AFTER}B written raw post-release; durable re-admission recorded"

    echo
    echo "✅ unfence-rig PASSED — the fence is REVERSIBLE, through the REAL operator"
    echo "   flow: fence a mid-write client, reboot the node (the only recovery from"
    echo "   its D-state pwrite), watch the restarted stack RE-ESTABLISH the fence"
    echo "   from the durable record, then UnfenceBlockClient cleared the record,"
    echo "   RELEASED the EA-RO reservation, the evicted client reconnected, and its"
    echo "   O_DIRECT write moved the device counter the fence had frozen. On $KREL."
    exit 0
  fi

  # ── T. TGT_RESTART=1: does PTPL survive a TARGET restart? ───────────
  # The landmine (design §5): a tgt restart drops every reservation from
  # memory — without PTPL it silently unfences everyone. Two ways the
  # fence can come back, both PROVEN here by the STARTUP re-fence line:
  #   TGT_RESTART=1        — the ptpl_file survives, so SPDK restores the
  #                          reservation on ns re-add (nvmf_ns_reservation
  #                          _restore); startup re-fence is a no-op
  #                          (registered=false acquired=false).
  #   TGT_RESTART=1 +      — the ptpl_file is DELETED (ptpl loss / a fresh
  #   PTPL_LOSS=1            disk), so nothing restores from disk; the
  #                          MDS's durable fenced_clients record drives a
  #                          real re-acquire at startup (registered=true
  #                          acquired=true). This is the whole point of
  #                          the durable record: the fence survives even
  #                          TOTAL target-state loss.
  # Either way it is the together-restart: kill BOTH, bring the tgt back
  # on the SAME disk image (lvstore+lvol auto-load), MDS reconcile +
  # startup fenced-set replay re-establish the fence.
  if [ "${TGT_RESTART:-0}" = "1" ]; then
    if [ "${PTPL_LOSS:-0}" = "1" ]; then
      echo "▶ TGT_RESTART + PTPL_LOSS: the durable record must re-fence when ptpl is GONE"
    else
      echo "▶ TGT_RESTART: PTPL must survive a target restart (together-restart)"
    fi
    PTPL_FILE="$RIG/flint-ptpl-$VOL.json"
    vsh "test -s $PTPL_FILE" \
      || fail "no non-empty ptpl_file at $PTPL_FILE — PTPL was never persisted; a tgt restart WOULD unfence"
    echo "· ptpl_file persisted: $(vsh "wc -c < $PTPL_FILE")B, rtype=$(vsh "grep -o '\"rtype\"[: ]*[0-9]*' $PTPL_FILE | head -1")"

    # Kill BOTH (together-restart). MDS first so it stops reconciling.
    vsudo "pkill -9 -x flint-pnfs-mds"
    vsudo "[ -f /var/tmp/spdk-rig.pid ] && kill -9 \$(cat /var/tmp/spdk-rig.pid) 2>/dev/null;
           pkill -9 -x reactor_0; pkill -9 -x spdk_tgt"
    for i in $(seq 1 20); do
      vsh "pgrep -x reactor_0 || pgrep -x spdk_tgt || pgrep -x flint-pnfs-mds" >/dev/null || break
      sleep 0.5
    done
    vsudo "rm -f /var/tmp/spdk_cpu_lock_*"

    # PTPL_LOSS: destroy the on-disk reservation. Now NOTHING target-side
    # carries the fence across the restart — only the MDS's durable
    # fenced_clients record (in sqlite) does.
    if [ "${PTPL_LOSS:-0}" = "1" ]; then
      vsudo "rm -f $PTPL_FILE"
      vsh "test -e $PTPL_FILE" && fail "ptpl_file not deleted" || echo "· ptpl_file DELETED — target-side reservation is now unrecoverable"
    fi

    # Bring the tgt back on the SAME disk image + SAME ptpl_dir — touch
    # NEITHER (that is the whole point). aio re-create auto-loads the
    # lvstore + lvol; NO bdev_lvol_create_lvstore (that would wipe).
    vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $SOCK -m 0x1 --wait-for-rpc >>$RIG/spdk.log 2>&1 &
           echo \$! > /var/tmp/spdk-rig.pid; sleep 0.5"
    for i in $(seq 1 20); do
      vsh "$RPC rpc_get_methods >/dev/null 2>&1" && break
      [ "$i" = 20 ] && fail "tgt RPC never came back after restart ($RIG/spdk.log)"
      sleep 0.5
    done
    vsudo "chmod 0777 $SOCK"
    vsh "$RPC iobuf_set_options --small-pool-count 4096 --large-pool-count 1024" || fail "iobuf (tgt restart)"
    vsh "$RPC iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1" || fail "iscsi (tgt restart)"
    vsh "$RPC framework_start_init" || fail "framework_start_init (tgt restart)"
    for i in $(seq 1 60); do
      vsh "$RPC framework_wait_init >/dev/null 2>&1" && break
      [ "$i" = 60 ] && fail "subsystems never initialized (tgt restart)"
      sleep 0.5
    done
    vsh "$RPC bdev_aio_create /var/tmp/rig-disk.img rigdisk 4096" >/dev/null || fail "bdev_aio_create (tgt restart)"
    for i in $(seq 1 40); do
      vsh "$RPC bdev_get_bdevs --name lvs_rig/$VOL >/dev/null 2>&1" && break
      [ "$i" = 40 ] && fail "the lvstore/lvol did NOT auto-load from the reattached disk image"
      sleep 0.5
    done
    vsh "$RPC nvmf_create_transport -t TCP" || fail "nvmf_create_transport (tgt restart)"
    echo "✓ tgt restarted; lvstore+lvol auto-loaded from the same disk image (memory wiped)"

    # MDS back → startup reconcile re-adds the ns (SPDK restores the
    # reservation from ptpl_file IF it survived), THEN the startup
    # fenced-set replay re-acquires EA-RO for every fenced volume from
    # the durable record. The STARTUP re-fence line is the proof, and it
    # discriminates the two paths.
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
    for i in $(seq 1 20); do
      vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
      [ "$i" = 20 ] && fail "MDS gRPC never came back after tgt restart"
      sleep 0.5
    done
    # Wait for the startup fenced-set replay to run (it logs one line per
    # re-established fence). This is the code path under test.
    STARTUP=""
    for i in $(seq 1 60); do
      STARTUP=$(vsh "grep 'startup re-fence' $RIG/mds.log | tail -1" || true)
      [ -n "$STARTUP" ] && break
      sleep 0.5
    done
    [ -n "$STARTUP" ] || fail "the startup fenced-set replay never ran — the durable record was not consulted"
    echo "✓ MDS restarted; startup fenced-set replay ran from the durable record"

    # THE PROOF, read off the STARTUP re-fence line:
    if [ "${PTPL_LOSS:-0}" = "1" ]; then
      # ptpl was deleted → nothing restored from disk → the durable
      # record drove a REAL re-acquire.
      echo "$STARTUP" | grep -q 'registered=true' \
        || fail "PTPL_LOSS: expected the durable record to RE-REGISTER, but it did not: ${STARTUP}"
      echo "$STARTUP" | grep -q 'acquired=true' \
        || fail "PTPL_LOSS: expected the durable record to RE-ACQUIRE EA-RO, but it did not: ${STARTUP}"
      RESULT="the durable fenced_clients record RE-ESTABLISHED the fence after ptpl LOSS"
    else
      # ptpl survived → SPDK restored on ns re-add → startup re-fence is
      # a no-op.
      echo "$STARTUP" | grep -q 'registered=false' \
        || fail "PTPL did NOT survive — the MDS had to re-register (reservation was lost): ${STARTUP}"
      echo "$STARTUP" | grep -q 'acquired=false' \
        || fail "PTPL did NOT survive — the MDS had to re-acquire EA-RO: ${STARTUP}"
      RESULT="PTPL restored the reservation from disk (startup re-fence was a no-op)"
    fi
    echo "$STARTUP" | grep -q 'rtype=0x4' \
      || fail "reservation is not EA-RO after the restart: ${STARTUP}"
    echo "$STARTUP" | grep -q '0x666c696e745f6d64(holder)' \
      || fail "the MDS key is not the reservation holder after the restart: ${STARTUP}"
    echo "✓ ${RESULT}: ${STARTUP}"

    CONN=$(vsudo "nvme connect -t tcp -a 127.0.0.1 -s 4420 -n $SUBNQN --hostnqn=$HOSTNQN --ctrl-loss-tmo=3 2>&1; true")
    echo "$CONN" | grep -qiE 'not allowed|Connect command failed|Input/output|Operation not permitted|refused' \
      || { vsudo "nvme disconnect -n $SUBNQN 2>/dev/null"; fail "the fenced client RE-CONNECTED after the restart: $CONN"; }
    vsudo "nvme disconnect -n $SUBNQN 2>/dev/null; true"
    echo "✓ fenced client still refused at the device after the restart"

    echo
    if [ "${PTPL_LOSS:-0}" = "1" ]; then
      echo "✅ durable-fenced-record PASSED — the tgt restarted AND its ptpl_file was"
      echo "   destroyed, yet the fence came back: the MDS's durable fenced_clients"
      echo "   record survived in sqlite and re-acquired the EA-RO reservation at"
      echo "   startup. The fence survives TOTAL target-state loss."
    else
      echo "✅ ptpl-survives-tgt-restart PASSED — a target restart wiped the tgt's"
      echo "   memory, yet the fence came back: the MDS reconcile re-added the ns with"
      echo "   its ptpl_file and SPDK RESTORED the EA-RO reservation from disk (the"
      echo "   startup re-fence found it already held). Without PTPL this unfences all."
    fi
    exit 0
  fi

  # ── R. RESTART=1: does the fence SURVIVE an MDS restart? ────────────
  # Reservation holdership is target-side, keyed to the MDS's STABLE
  # NVMe Host Identifier (identity.rs BLOCK_MDS_HOST_ID / _PR_KEY are
  # constants, not per-boot). The eviction is in sqlite (block_hosts).
  # So the claim is: kill+restart the MDS, and the fence is UNCHANGED —
  # the restarted MDS re-establishes holdership WITHOUT re-registering or
  # re-acquiring (a no-op re-acquire is the correct answer for an
  # MDS-only restart: PTPL/tgt-memory persisted the reservation, and the
  # stable identity means the tgt still sees the MDS as the holder).
  if [ "${RESTART:-0}" = "1" ]; then
    echo "▶ RESTART: killing + restarting the MDS with the fence active"
    vsudo "pkill -9 -x flint-pnfs-mds"
    for i in $(seq 1 20); do
      vsh "pgrep -x flint-pnfs-mds" >/dev/null || break
      sleep 0.5
    done
    # Same launch as §3, APPENDING to the log so the pre-restart fence
    # lines survive for the assertions.
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
    for i in $(seq 1 20); do
      vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
      [ "$i" = 20 ] && fail "MDS gRPC never came back after restart"
      sleep 0.5
    done
    # R1. startup replay ran — the export chain re-converged from sqlite.
    vsh "grep -q 'block-export.*reconcile.*converged\|block-export startup replay' $RIG/mds.log" \
      || fail "restarted MDS did not run the block-export startup replay"
    echo "✓ MDS restarted; startup replay converged the export from sqlite"

    # R2. the durable eviction SURVIVED: the fenced client is NOT on the
    # re-converged allow-list — only the MDS fence-lane host remains.
    HOSTS=$(vsh "$RPC nvmf_get_subsystems" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(' '.join(h['nqn'] for h in s.get('hosts', []))); break
")
    echo "$HOSTS" | grep -q "$HOSTNQN" \
      && fail "the fenced client's host was RE-ADMITTED after restart: $HOSTS"
    echo "$HOSTS" | grep -q ':mds:resv-fence' \
      || fail "the MDS fence-lane host is missing from the allow-list after restart: $HOSTS"
    echo "✓ durable eviction survived: allow-list = [$HOSTS] (fenced client absent, fence lane kept)"

    # R3. re-run the fence lever. If the reservation SURVIVED, the
    # restarted MDS (stable identity) finds itself ALREADY the registrant
    # AND holder — so registered=false, acquired=false, and it is still
    # the EA-RO holder. That no-op re-acquire IS the re-acquire path.
    MARK=$(vsh "wc -l < $RIG/mds.log")
    FR2=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
          -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID\"}' \
          127.0.0.1:50051 pnfs.control.MdsControl/FenceBlockClient") \
      || fail "post-restart FenceBlockClient RPC failed"
    echo "$FR2" | grep -c '"fenced": true' >/dev/null || fail "post-restart fence refused: $FR2"
    # The fence-preempt DETAIL line carries registered=/acquired= AND the
    # reservation state. Anchor on 'registered=' — it is unique to that
    # line ('reservation preempt' alone also matches the ...preempted
    # summary line, which lacks these fields).
    RESV2=$(vsh "tail -n +$MARK $RIG/mds.log | grep 'registered=' | tail -1" || true)
    echo "$RESV2" | grep -q 'registered=false' \
      || fail "post-restart re-fence RE-REGISTERED — identity was NOT stable across restart: ${RESV2:-<none>}"
    echo "$RESV2" | grep -q 'acquired=false' \
      || fail "post-restart re-fence RE-ACQUIRED — the reservation did NOT survive the restart: ${RESV2:-<none>}"
    echo "$RESV2" | grep -q 'rtype=0x4' \
      || fail "post-restart reservation is not EA-RO: ${RESV2:-<none>}"
    echo "$RESV2" | grep -q '0x666c696e745f6d64(holder)' \
      || fail "the MDS key is not the reservation holder after restart: ${RESV2:-<none>}"
    echo "✓ reservation SURVIVED the restart: ${RESV2}"

    # R4. the client is still fenced at the device: a fresh nvme connect
    # for its hostnqn is refused by the (durable) allow-list.
    CONN=$(vsudo "nvme connect -t tcp -a 127.0.0.1 -s 4420 -n $SUBNQN --hostnqn=$HOSTNQN --ctrl-loss-tmo=3 2>&1; true")
    echo "$CONN" | grep -qiE 'not allowed|Connect command failed|Input/output|Operation not permitted|refused' \
      || { vsudo "nvme disconnect -n $SUBNQN 2>/dev/null"; fail "the fenced client RE-CONNECTED after restart: $CONN"; }
    vsudo "nvme disconnect -n $SUBNQN 2>/dev/null; true"
    echo "✓ fenced client still refused at the device after restart"

    echo
    echo "✅ fence-restart-rig PASSED — the fence SURVIVED an MDS restart: the"
    echo "   reservation persisted (no re-register, no re-acquire — the stable MDS"
    echo "   identity reclaimed holdership), the sqlite eviction survived, and the"
    echo "   fenced client stayed out. This is the MdsRestart re-acquire path."
    exit 0
  fi

  echo
  echo "✅ fence-rig PASSED — a reservation held by the MDS (EA-RO, PTPL) stopped a"
  echo "   LIVE raw-path writer's bytes at the device on kernel $KREL (FenceReaches"
  echo "   PROVEN for this tgt), the failure reached the client, and the client"
  echo "   returned its layout. Reclaim-after-fence: see the base rig + unit tests."
  exit 0
fi

# ── C. RECONCILE=1: a tgt-ONLY restart repairs WITHOUT an MDS roll ───
# The gap this proves closed: an MDS restart replays exports at
# startup, but a tgt that restarted UNDER a running MDS came back
# empty and STAYED empty — the runbook's answer was "roll the MDS".
# Now the periodic export reconcile loop (FLINT_PNFS_EXPORT_RECONCILE_
# SECS, here $RECON_SECS s) must rebuild subsystem/namespace/listener/
# allow-list from sqlite within one interval, with the MDS pid
# UNCHANGED, and the client's surviving kernel controller (CTRL_LOSS
# raised for this mode — the production shape; the 1800s default dwarfs
# any tgt restart) reconnects and writes raw again. Falls through to
# the REMOVE reclaim + unstage/detach, so the whole chain is proven
# healthy POST-repair, not merely present.
if [ "${RECONCILE:-0}" = "1" ]; then
  MDS_PID=$(vsh "pgrep -x flint-pnfs-mds" | head -1)
  [ -n "$MDS_PID" ] || fail "no running MDS to hold steady"
  echo "▶ RECONCILE: killing the tgt ONLY (MDS pid $MDS_PID keeps running)"
  vsudo "[ -f /var/tmp/spdk-rig.pid ] && kill -9 \$(cat /var/tmp/spdk-rig.pid) 2>/dev/null;
         pkill -9 -x reactor_0; pkill -9 -x spdk_tgt"
  for i in $(seq 1 20); do
    vsh "pgrep -x reactor_0 || pgrep -x spdk_tgt" >/dev/null || break
    [ "$i" = 20 ] && fail "old tgt never died"
    sleep 0.5
  done
  vsudo "rm -f /var/tmp/spdk_cpu_lock_* $SOCK ${SOCK}.lock"

  # Same-disk restart: lvstore+lvol auto-load; NO create_lvstore.
  vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $SOCK -m 0x1 --wait-for-rpc >>$RIG/spdk.log 2>&1 &
         echo \$! > /var/tmp/spdk-rig.pid; sleep 0.5"
  for i in $(seq 1 20); do
    vsh "$RPC rpc_get_methods >/dev/null 2>&1" && break
    [ "$i" = 20 ] && fail "tgt RPC never came back ($RIG/spdk.log)"
    sleep 0.5
  done
  vsudo "chmod 0777 $SOCK"
  vsh "$RPC iobuf_set_options --small-pool-count 4096 --large-pool-count 1024" || fail "iobuf (restart)"
  vsh "$RPC iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1" || fail "iscsi (restart)"
  vsh "$RPC framework_start_init" || fail "framework_start_init (restart)"
  for i in $(seq 1 60); do
    vsh "$RPC framework_wait_init >/dev/null 2>&1" && break
    [ "$i" = 60 ] && fail "subsystems never initialized (restart)"
    sleep 0.5
  done
  vsh "$RPC bdev_aio_create /var/tmp/rig-disk.img rigdisk 4096" >/dev/null || fail "bdev_aio_create (restart)"
  for i in $(seq 1 40); do
    vsh "$RPC bdev_get_bdevs --name lvs_rig/$VOL >/dev/null 2>&1" && break
    [ "$i" = 40 ] && fail "the lvstore/lvol did NOT auto-load after the restart"
    sleep 0.5
  done
  vsh "$RPC nvmf_create_transport -t TCP" || fail "nvmf_create_transport (restart)"
  echo "✓ tgt back on the same disk, EMPTY of subsystems — the MDS was not touched"

  # C1. the LOOP repairs: subsystem + allow-list reappear within a few
  # intervals, with nobody restarting the MDS.
  REPAIRED=0
  for i in $(seq 1 12); do
    HOSTS=$(vsh "$RPC nvmf_get_subsystems 2>/dev/null" | python3 -c "
import json,sys
try:
    for s in json.load(sys.stdin):
        if s.get('nqn') == '$SUBNQN':
            print(' '.join(h['nqn'] for h in s.get('hosts', []))); break
except Exception: pass
" || true)
    if echo "$HOSTS" | grep -c "$HOSTNQN" >/dev/null; then REPAIRED=1; break; fi
    sleep 5
  done
  [ "$REPAIRED" = "1" ] || fail "the reconcile loop never rebuilt $SUBNQN (+$HOSTNQN) — hosts: '${HOSTS:-none}'"
  MDS_PID2=$(vsh "pgrep -x flint-pnfs-mds" | head -1)
  [ "$MDS_PID2" = "$MDS_PID" ] || fail "MDS pid changed ($MDS_PID → $MDS_PID2) — this proved a restart, not the loop"
  echo "✓ reconcile loop rebuilt the export from sqlite (MDS pid $MDS_PID unchanged): [$HOSTS]"

  # C2. bytes flow again through the SURVIVING controller: the kernel
  # initiator reconnects on its own (reconnect-delay=2, ctrl-loss
  # $CTRL_LOSS) once the allow-list is back; the restart zeroed the
  # device counter, so everything it counts now is post-repair traffic.
  RESUME_MIB=8
  WROTE=0
  for i in $(seq 1 12); do
    if vsudo "dd if=/dev/urandom of=$MNT/data.bin bs=1M count=$RESUME_MIB \
              oflag=direct conv=notrunc status=none && sync $MNT/data.bin" 2>/dev/null; then
      WROTE=1; break
    fi
    sleep 5
  done
  [ "$WROTE" = "1" ] || fail "post-repair O_DIRECT write never succeeded — the client did not recover"
  W_AFTER=$(lvol_written)
  NEED_RW=$((RESUME_MIB * 1024 * 1024))
  [ "${W_AFTER:-0}" -ge "$NEED_RW" ] \
    || fail "device saw only ${W_AFTER}B post-repair (need ≥$NEED_RW) — the write did not go raw"
  echo "✓ client recovered without any node-side action: ${W_AFTER}B written raw post-repair"
  echo "· falling through to REMOVE reclaim + unstage/detach on the repaired stack"
fi

# ── 10. REMOVE → clean reclaim (return, not quarantine) ──────────────
# Every extent frees via the client's LAYOUTRETURN — zero quarantine
# tolerated.
vsudo "rm $MNT/data.bin"
LEFT=1
for i in $(seq 1 30); do
  LEFT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM extents WHERE volume='$VOL'\"" || echo 1)
  [ "${LEFT:-1}" = "0" ] && break
  sleep 0.5
done
QUAR=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
  \"SELECT COUNT(*) FROM extent_quarantine WHERE volume='$VOL'\"" || echo "?")
[ "${LEFT:-1}" = "0" ] || fail "extent rows never drained after REMOVE ($LEFT left, quarantine=$QUAR)"
[ "${QUAR:-1}" = "0" ] || fail "REMOVE quarantined $QUAR range(s) — expected clean frees via LAYOUTRETURN"
echo "✓ REMOVE reclaimed every extent cleanly (0 rows, 0 quarantined)"

# ── 11. unstage + detach — the teardown half of session management ───
# NodeUnstage's inverse (unstage: link removed, session disconnected)
# then ControllerUnpublish's (detach: the durable node-attach row goes,
# and a replay reports itself instead of erroring).
vsudo "umount $MNT" || fail "umount before unstage"
UNSTAGE=$(vsudo "$CSI_CLI unstage --volume-id $VOL") || fail "pnfs-csi-cli unstage"
echo "$UNSTAGE" | grep -c 'disconnected=true' >/dev/null || fail "unstage did not disconnect: $UNSTAGE"
echo "$UNSTAGE" | grep -c 'link_removed=true' >/dev/null || fail "unstage left the eui link: $UNSTAGE"
vsh "test -e /dev/disk/by-id/nvme-eui.$NGUID" \
  && fail "eui link still present after unstage (the dangling-link landmine)"
vsh "$CSI_CLI detach --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)" >/dev/null \
  || fail "pnfs-csi-cli detach"
NAROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
  \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"" || echo "?")
[ "${NAROWS:-1}" = "0" ] || fail "detach left $NAROWS node-attach row(s)"
DETACH2=$(vsh "$CSI_CLI detach --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
  || fail "detach replay errored"
echo "$DETACH2" | grep -c 'replay' >/dev/null || fail "detach replay did not self-identify: $DETACH2"
echo "✓ unstage+detach: session down, link gone, attach row swept, replay clean"

echo
if [ "${RECONCILE:-0}" = "1" ]; then
  echo "✅ reconcile-rig PASSED — a tgt-ONLY restart was repaired by the periodic"
  echo "   export reconcile loop with the MDS untouched (pid unchanged): exports"
  echo "   rebuilt from sqlite, the client reconnected on its own, raw bytes flowed,"
  echo "   and REMOVE reclaim + unstage/detach ran clean on the repaired stack."
elif [ "${FENCE:-0}" = "1" ]; then
  echo "✅ fence-rig PASSED — a reservation preempt from the MDS stopped a live"
  echo "   raw-path writer's bytes at the device on kernel $KREL (FenceReaches PROVEN"
  echo "   for this tgt), and the failure surfaced to userspace as an error."
else
  echo "✅ block-rig PASSED — a stock $KREL kernel client did raw-extent NVMe I/O"
  echo "   through LAYOUTGET(5)/GETDEVICEINFO/LAYOUTCOMMIT against flint's MDS+spdk-tgt."
fi
