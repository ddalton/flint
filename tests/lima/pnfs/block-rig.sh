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
# SWEEP=1 — the lease-sweep partition drill (§P): DROP the NFS port
#   under a live raw-path writer (metadata dead, data alive — the
#   zombie), the sweep fences/revokes/auto-unfences ON THE TIMER, the
#   node reboots, and a successor stages + writes with zero levers.
# PREEMPT=1 — the foreign-holder fence arm (§X), the branch every
#   earlier fence left cold: the client REGISTERS a key and ACQUIRES
#   Write Exclusive (no conforming kernel does either), the fence
#   preempts the holder / takes EA-RO / wipes the key / freezes the
#   writer — and a second volume on the same tgt, staged by the same
#   host, keeps writing raw (per-namespace reservation, per-subsystem
#   eviction).
# MULTI=1 — TWO REAL CLIENT HOSTS on one volume (§M): $VM2 stages and
#   mounts the SAME volume through the host proxy, both hosts write raw
#   at once, and the drill asserts what only two hosts can show —
#   admission is additive, the extent map stays physically disjoint
#   across hosts (GrantsExclusive on real HW), same-file contention is
#   refused rather than overlapped, and a fence naming ONE client
#   evicts only that client. Whether the volume-wide reservation is
#   collateral for the survivor is asserted BOTH ways.
# EXPAND=1 — capacity semantics for real (§E): a small volume is FILLED
#   until the app gets ENOSPC (not EIO — the errno an application can
#   act on), then ExpandVolume grows the lvol, the client kernel picks
#   the bigger namespace up through SPDK's resize AEN, the arena ceiling
#   rises, CB_NOTIFY_DEVICEID drops the client's cached pNFS device, and
#   the SAME MOUNT writes past the old ceiling sha-intact — no remount,
#   which is the property the notification exists for.
# ZOMBIE=1 — the frozen-VM zombie (§Z), FlintAdmission's only dangerous
#   shape: a SECOND lima VM (ZOMBIE_VM, default flint-zombie) stages
#   through the production attach + a proxied cross-VM session, writes
#   raw, and is SIGSTOPped at the hypervisor mid-write. The sweep
#   fences/revokes/auto-unfences the frozen client; a successor on THIS
#   VM reuses its extents; the zombie resumes — and the successor's
#   sha-checked bytes must survive whatever its kernel does next.
#   Needs: flint-zombie VM with the HWE kernel (see §Z0's hint).
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
# Lease sweep cadence + server grace, also rig-tightened on every MDS
# launch (production: 30s sweep, 90s grace). The sweep's boot hold is
# lease_time (90s, fixed) + grace — a short grace keeps the drill's
# total wait inside one coffee. Uniform across modes: a LIVE client is
# never swept (aliveness gate), and the fence modes' assertions all
# complete inside the ~95s boot hold.
SWEEP_SECS=5
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
  # ZOMBIE=1 plumbing first: a failed run must never leave the zombie
  # VM's hypervisor SIGSTOPped (an invisible frozen VM eating RAM) or
  # the host proxy squatting on its ports.
  if [ "${ZOMBIE:-0}" = "1" ]; then
    kill -CONT $(pgrep -f "\.lima/${ZOMBIE_VM:-flint-zombie}|hostagent.*${ZOMBIE_VM:-flint-zombie}") 2>/dev/null
    [ -f /tmp/flint-zombie-proxy.pid ] && kill "$(cat /tmp/flint-zombie-proxy.pid)" 2>/dev/null
    rm -f /tmp/flint-zombie-proxy.pid
  fi
  # The FENCE=1 continuous writer FIRST: a leftover dd-loop from a
  # failed fence run keeps writing $MNT/data.bin, and the next run's
  # step-8 urandom write races it → a phantom sha mismatch that looks
  # like data corruption but is just a ghost from the last run.
  vsudo "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null;
         pkill -9 -f 'of=$MNT/data.bin'; pkill -9 -f rig-writer.py;
         rm -f /var/tmp/rig-writer.pid /var/tmp/rig-writer.done /var/tmp/rig-writer.err" \
    >/dev/null 2>&1
  # Both mounts: EXPAND=1's own volume is mounted at $MNT-e, and a run
  # that died before its teardown leaves a STALE handle there — the next
  # run's `mkdir -p` then fails with ESTALE before it can even mount
  # (rig-found, expand run 2). Unconditional: a leftover from an EXPAND
  # run must not wedge an unrelated one.
  vsudo "umount -lf $MNT 2>/dev/null; umount -lf ${MNT}-e 2>/dev/null;
         nvme disconnect -n $SUBNQN 2>/dev/null;
         nvme disconnect -n ${SUBNQN}-e 2>/dev/null;
         rm -rf /var/lib/kubelet/plugins/flint.csi.storage.io/block-sessions" >/dev/null 2>&1
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
vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='$MDS_LOG' nohup $MDS_BIN --config $CFG >$RIG/mds.log 2>&1 & sleep 0.5"
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

# ── 4b. the capacity gate, against a REAL lvolstore ──────────────────
# SPDK will not refuse an oversubscribed thin provision on its own —
# `blob_resize` skips its free-cluster check entirely for thin blobs
# (lib/blob/blobstore.c:2292) — so without our gate the create succeeds,
# the PVC reports its full size, and the application discovers the truth
# at WRITE time as an lvol-level error. Unit tests prove the arithmetic
# against a fake; only the rig proves that a real `bdev_lvol_get_lvstores`
# reports what we read it as. The store is 1 GiB with 512 MiB already
# promised above, so 4 GiB cannot be funded.
OVER=$(vsh "$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
      -d '{\"volumeId\":\"rigvol-toobig\",\"sizeBytes\":$((4 * 1024 * 1024 * 1024)),\"layoutClass\":\"scsi\"}' \
      127.0.0.1:50051 pnfs.control.MdsControl/CreateVolume") || fail "CreateVolume (oversize) RPC"
[ "$(echo "$OVER" | grep -c '"created": true' || true)" = "0" ] \
  || fail "a 4GiB volume was ACCEPTED on a 1GiB lvolstore — the capacity gate did not fire: $OVER"
[ "$(echo "$OVER" | grep -c 'promised' || true)" -ge 1 ] \
  || fail "the refusal does not name the promise budget: $OVER"
# ...and it refused BEFORE touching the device.
vsh "$RPC bdev_get_bdevs --name lvs_rig/rigvol-toobig" >/dev/null 2>&1 \
  && fail "the refused volume left an lvol behind — the gate fired after the create"
echo "✓ capacity gate: 4GiB refused on a 1GiB store, no lvol created"

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

# ── 6c. session re-establishment (ctrl_loss exhaustion) ──────────────
# A controller whose ctrl_loss_tmo expires during a long outage is
# DELETED kernel-side, and kubelet re-runs NodeStage only after a
# reboot — the reconcile pass (node agent, 30s; here driven once via
# the CLI) re-establishes it from the durable session record stage
# wrote. Simulate the exhaustion with a hard disconnect, then one pass
# must bring the session, device, and link back.
vsudo "nvme disconnect -n $SUBNQN" >/dev/null || fail "disconnect for the exhaustion drill"
vsh "test -b /dev/$NSDEV" && fail "device still present after disconnect"
RE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
     $CSI_CLI reestablish") || fail "reestablish pass errored: $RE"
echo "$RE" | grep -c 'repaired=1' >/dev/null || fail "pass did not repair the session: $RE"
# The device may renumber across the fresh connect — re-resolve from
# the (re-ensured) eui link, and use THAT name from here on.
NSDEV=$(basename "$(vsh "readlink -f /dev/disk/by-id/nvme-eui.$NGUID")")
vsh "test -b /dev/$NSDEV" || fail "no block device behind the eui link after reestablish"
# A second pass is a no-op (controller present → not ours to touch).
RE2=$(vsudo "$CSI_CLI reestablish") || fail "no-op pass errored"
echo "$RE2" | grep -c 'repaired=0' >/dev/null || fail "no-op pass repaired something: $RE2"
echo "✓ session re-established from the durable record: /dev/$NSDEV (second pass no-op)"

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

# ── R. the roller's read: BlockExportStatus sees the LIVE initiator ───
# The maintenance roller refuses to roll a node whose spdk-tgt has block
# clients on it, and this RPC is the only thing that tells it so. The
# planner is pure and unit-tested; what needs REAL hardware is the claim
# underneath it — that an actual kernel nvme session, doing actual raw
# I/O right now, shows up here. A green unit suite over a source that
# reports nobody is the bug wearing a passing test.
# `grep -c`, never `grep -q`: under `set -o pipefail` a -q that exits on
# the first match can SIGPIPE the writer and turn a MATCH into a failed
# pipeline (the runak lesson — and it bit this very section on its first
# run, which is exactly how cheap that mistake is to repeat).
BS=$(vsh "$CSI_CLI block-status --endpoint 127.0.0.1:50051") || fail "block-status RPC"
[ "$(echo "$BS" | grep -c 'enabled=true' || true)" -ge 1 ] \
  || fail "block-status says the shard serves no block class: $BS"
BS_N=$(echo "$BS" | head -1 | sed -n 's/.*initiators=\([0-9]*\).*/\1/p')
[ "${BS_N:-0}" -ge 1 ] \
  || fail "block-status reports $BS_N initiators while this host is mounted and writing — the roller would roll this tgt: $BS"
# Match on the NQN, not on a second `vsh hostname` round-trip: HOSTNQN is
# the identity the allow-list and the roller both key on, and it is
# already derived from this host above.
[ "$(echo "$BS" | grep -c "source=attach nqn=$HOSTNQN" || true)" -ge 1 ] \
  || fail "block-status does not name THIS host's attachment ($HOSTNQN): $BS"
echo "✓ BlockExportStatus sees the live session: $BS_N initiator(s), this host named"

lvol_written() {
  vsh "$RPC bdev_get_iostat --name lvs_rig/$VOL" | python3 -c "
import json,sys; print(json.load(sys.stdin)['bdevs'][0]['bytes_written'])"
}

# ── the SECOND CLIENT VM, shared by every multi-host mode ────────────
# lima VMs cannot dial each other (each is 192.168.5.15 on its own
# user-net), so a second client reaches this rig through
# host.lima.internal + tcp-proxy.py on the host + lima's auto-forward of
# THIS VM's loopback listeners. One implementation, because the last
# same-message-two-places bug in this tree (the recall seqid) cost a
# month of silently-refused callbacks.
VM2="${VM2:-${ZOMBIE_VM:-flint-zombie}}"
MNT2=/mnt/flint-vm2
PXY_NVME=24420; PXY_NFS=20491; PXY_GRPC=50052
PXY_PID_FILE=/tmp/flint-zombie-proxy.pid
vsh2()   { limactl shell "$VM2" -- bash -c "$*"; }
vsudo2() { limactl shell "$VM2" -- sudo bash -c "$*"; }
vm2_procs() { pgrep -f "\.lima/$VM2|hostagent.*$VM2" || true; }

# Boot VM2 clean. A prior run may have left a D-state writer on a dead
# device, and reboot is the only reliable sweep for that.
vm2_boot() {
  limactl list 2>/dev/null | grep -q "^$VM2 " \
    || fail "no lima VM '$VM2' — create it: limactl create --name=$VM2 --cpus=2 --memory=2 --disk=10 template://ubuntu-24.04, then apt-get install -y linux-generic-hwe-24.04 nvme-cli nfs-common && reboot"
  limactl stop -f "$VM2" >/dev/null 2>&1 || true
  limactl start "$VM2" --tty=false >/dev/null 2>&1 || fail "could not start $VM2"
  VM2_KREL=$(vsh2 "uname -r")
  case "$VM2_KREL" in
    [0-5].*|6.[0-9].*|6.10.*) fail "$VM2 kernel $VM2_KREL < 6.11 — install linux-generic-hwe-24.04" ;;
  esac
  echo "✓ second client VM up on $VM2_KREL (rebooted clean)"
}

# Host-side proxy: re-export this VM's forwarded loopback ports on
# 0.0.0.0, where VM2's host.lima.internal can reach them.
vm2_proxy_up() {
  [ -f "$PXY_PID_FILE" ] && { kill "$(cat $PXY_PID_FILE)" 2>/dev/null || true; rm -f "$PXY_PID_FILE"; }
  python3 "$REPO_ROOT/tests/lima/pnfs/tcp-proxy.py" \
    "$PXY_NVME:4420" "$PXY_NFS:20490" "$PXY_GRPC:50051" >/tmp/flint-zombie-proxy.log 2>&1 &
  echo $! > "$PXY_PID_FILE"
  for i in $(seq 1 10); do
    grep -q ready /tmp/flint-zombie-proxy.log && break
    [ "$i" = 10 ] && fail "tcp-proxy never bound ($(cat /tmp/flint-zombie-proxy.log))"
    sleep 0.5
  done
  vsh2 "timeout 5 bash -c '</dev/tcp/host.lima.internal/$PXY_GRPC'" \
    || fail "$VM2 cannot reach the rig through the host proxy"
  # ASCII arrows only next to $vars: macOS bash 3.2 under set -u parses
  # a glued multibyte char INTO the variable name -> unbound-variable.
  echo "✓ host proxy up (nvme $PXY_NVME->4420, nfs $PXY_NFS->20490, grpc $PXY_GRPC->50051)"
}

# Stage VM2 onto $1 and mount it at $2. Attach through the production verb
# (per-node admission for VM2's OWN hostnqn), then a MANUAL connect: the
# attach-answered traddr is rig-loopback truth (127.0.0.1), which is a
# DIFFERENT HOST from VM2's seat, so the drill dials the proxy instead.
# Sets HOSTNQN2 / NGUID2 / SUBNQN2 / NSDEV2.
vm2_stage() {
  local vol="$1" mnt="$2"
  vsudo2 "modprobe nvme-tcp && modprobe blocklayoutdriver" \
    || fail "$VM2 cannot load nvme-tcp/blocklayoutdriver"
  local att
  att=$(vsh2 "$CSI_CLI attach --endpoint host.lima.internal:$PXY_GRPC --volume-id $vol --node \$(hostname)") \
    || fail "$VM2 attach failed"
  HOSTNQN2=$(echo "$att" | python3 -c 'import json,sys; print(json.load(sys.stdin)["hostNqn"])')
  NGUID2=$(echo "$att" | python3 -c 'import json,sys; print(json.load(sys.stdin)["nguid"])')
  SUBNQN2=$(echo "$att" | python3 -c 'import json,sys; print(json.load(sys.stdin)["subnqn"])')
  vsudo2 "nvme connect -t tcp -a host.lima.internal -s $PXY_NVME -n $SUBNQN2 -q $HOSTNQN2 -l 600 -c 2" \
    || fail "$VM2 nvme connect refused"
  NSDEV2=""
  for i in $(seq 1 20); do
    NSDEV2=$(vsh2 "for d in /sys/class/block/nvme*n*; do case \$(basename \$d) in *c*n*) continue;; esac;
      g=\$(cat \$d/nguid 2>/dev/null | tr -d -); [ \"\$g\" = \"$NGUID2\" ] && basename \$d && break; done" || true)
    [ -n "$NSDEV2" ] && break
    sleep 0.5
  done
  [ -n "$NSDEV2" ] || fail "$VM2 namespace for NGUID $NGUID2 never appeared"
  vsudo2 "ln -sf /dev/$NSDEV2 /dev/disk/by-id/nvme-eui.$NGUID2
          for f in /sys/class/nvme/*/fast_io_fail_tmo; do echo 5 > \$f; done"
  vsudo2 "mkdir -p $mnt && mount -t nfs4 -o vers=4.2,proto=tcp,port=$PXY_NFS host.lima.internal:/$vol $mnt" \
    || fail "$VM2 mount failed"
  echo "✓ $VM2 staged + mounted: /dev/$NSDEV2 as $HOSTNQN2, via the proxy"
}

# The NFS client id the MDS admitted for a host NQN on this volume —
# what a per-client fence has to name.
client_id_of_nqn() {
  vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT client_id FROM block_hosts WHERE volume='$VOL' AND host_nqn='$1' LIMIT 1\"" || true
}

# ── M. MULTI=1: TWO REAL CLIENT HOSTS on ONE volume ──────────────────
# Everything before this drill had exactly one client host. The
# properties that only exist with two — and that the model states but
# nothing had ever run — are:
#   M1. admission is ADDITIVE: two nodes, two NQNs, both on the export's
#       allow-list at once (a per-node admission that replaced instead
#       of adding would look identical with one node).
#   M2. two clients writing DIFFERENT files both reach the DEVICE, and
#       their extents are PHYSICALLY DISJOINT — GrantsExclusive
#       (FlintExtents' core theorem) on real hardware, across hosts.
#   M3. two clients over the SAME file: the second is REFUSED, not
#       given overlapping space, and the first client's bytes survive.
#   M4. the fence is PER CLIENT, not per volume: fencing host B must
#       stop B and leave A's admission intact. Whether A's raw I/O
#       survives is the open question this drill exists to answer — the
#       reservation is volume-wide at the device — so BOTH outcomes are
#       asserted, neither is silent, and either way A must be healthy
#       once the fence lifts.
# Topology: this VM keeps MDS+tgt and plays host A; $VM2 is host B,
# reaching the rig through the host proxy (see the VM2 helpers).
if [ "${MULTI:-0}" = "1" ]; then
  MNTB="$MNT2"
  vm2_boot
  vm2_proxy_up
  vm2_stage "$VOL" "$MNTB"
  [ "$NGUID2" = "$NGUID" ] || fail "host B attach NGUID '$NGUID2' != '$NGUID'"
  HOSTNQN_A="$HOSTNQN"

  # M1. BOTH hosts admitted at once.
  ATTACH_ROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"")
  [ "${ATTACH_ROWS:-0}" -ge 2 ] \
    || fail "expected 2 node-attach rows, found ${ATTACH_ROWS:-0} — per-node admission REPLACED instead of adding"
  HOSTS=$(vsh "$RPC nvmf_get_subsystems" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(' '.join(h['nqn'] for h in s.get('hosts', []))); break
")
  echo "$HOSTS" | grep -q "$HOSTNQN_A" || fail "host A's NQN missing from the allow-list: $HOSTS"
  echo "$HOSTS" | grep -q "$HOSTNQN2"  || fail "host B's NQN missing from the allow-list: $HOSTS"
  echo "✓ M1: both hosts admitted — allow-list carries A, B and the fence lane"

  # M1b. The roller's read is additive too. This is the number the
  # refusal message quotes ("N initiator(s)"), and with two real hosts
  # it is the first time it can be wrong in the direction that matters:
  # an under-count would let a campaign roll a tgt believing one client
  # would be hurt when two would.
  BSM=$(vsh "$CSI_CLI block-status --endpoint 127.0.0.1:50051") || fail "M1b block-status"
  BSM_N=$(echo "$BSM" | head -1 | sed -n 's/.*initiators=\([0-9]*\).*/\1/p')
  [ "${BSM_N:-0}" -ge 2 ] \
    || fail "block-status counts ${BSM_N:-0} initiator(s) with TWO hosts attached: $BSM"
  [ "$(echo "$BSM" | grep -c "nqn=$HOSTNQN_A" || true)" -ge 1 ] \
    || fail "host A missing from block-status: $BSM"
  [ "$(echo "$BSM" | grep -c "nqn=$HOSTNQN2" || true)" -ge 1 ] \
    || fail "host B missing from block-status: $BSM"
  echo "✓ M1b: the roller's read counts BOTH hosts ($BSM_N initiators, A and B named)"

  # M2. concurrent raw writes to DIFFERENT files.
  M_MIB=8
  vsh  "dd if=/dev/urandom of=/var/tmp/rig-a.bin bs=1M count=$M_MIB status=none"
  vsh2 "dd if=/dev/urandom of=/var/tmp/rig-b.bin bs=1M count=$M_MIB status=none"
  SHA_A=$(vsh  "sha256sum /var/tmp/rig-a.bin" | awk '{print $1}')
  SHA_B=$(vsh2 "sha256sum /var/tmp/rig-b.bin" | awk '{print $1}')
  MW0=$(lvol_written)
  BELT0=$(vsh "grep -c 'MDS I/O on scsi-class file' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  vsudo  "cp /var/tmp/rig-a.bin $MNT/a.bin && sync $MNT/a.bin"   || fail "host A write failed"
  vsudo2 "cp /var/tmp/rig-b.bin $MNTB/b.bin && sync $MNTB/b.bin" || fail "host B write failed"
  MW1=$(lvol_written)
  [ "$((MW1 - MW0))" -ge "$((2 * M_MIB * 1024 * 1024))" ] \
    || fail "device saw only $((MW1 - MW0))B for two ${M_MIB}MiB writes — someone went through the MDS"
  BELT1=$(vsh "grep -c 'MDS I/O on scsi-class file' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  [ "${BELT1:-0}" = "${BELT0:-0}" ] \
    || fail "the zeros belt fired during the concurrent writes ($BELT0 -> $BELT1)"

  # Cold read back on the OTHER host each time: A reads B's file and
  # vice versa, so the check crosses hosts as well as the device.
  vsudo  "echo 3 > /proc/sys/vm/drop_caches"
  vsudo2 "echo 3 > /proc/sys/vm/drop_caches"
  SHA_B_ON_A=$(vsudo  "sha256sum $MNT/b.bin"  | awk '{print $1}')
  SHA_A_ON_B=$(vsudo2 "sha256sum $MNTB/a.bin" | awk '{print $1}')
  [ "$SHA_B_ON_A" = "$SHA_B" ] || fail "host A read host B's file wrong: $SHA_B_ON_A != $SHA_B"
  [ "$SHA_A_ON_B" = "$SHA_A" ] || fail "host B read host A's file wrong: $SHA_A_ON_B != $SHA_A"

  # GrantsExclusive at the physical layer: no two extents of this volume
  # may overlap, whoever they were granted to. The allocator's own
  # verifier says this; here two real kernels on two hosts say it.
  OVERLAP=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM extents e1 JOIN extents e2
        ON e1.volume = e2.volume AND e1.rowid <> e2.rowid
       WHERE e1.volume='$VOL'
         AND e1.physical_offset < e2.physical_offset + e2.length
         AND e2.physical_offset < e1.physical_offset + e1.length\"")
  [ "${OVERLAP:-1}" = "0" ] \
    || fail "$OVERLAP overlapping physical extent pair(s) across two hosts — GrantsExclusive VIOLATED"
  FILES=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(DISTINCT file_id) FROM extents WHERE volume='$VOL'\"")
  echo "✓ M2: both hosts wrote raw ($((MW1 - MW0))B), cross-read intact, $FILES file(s), zero physical overlap"

  # M3. the SAME file from both hosts. Host A holds a live layout on
  # a.bin (held-open O_DIRECT writer); host B's write to the same file
  # must NOT be handed overlapping space. A refusal is the correct
  # answer — the belt's EIO or a TRYLATER loop — and A's data must be
  # intact afterwards either way.
  vsudo "rm -f /var/tmp/rig-writer.done
         nohup python3 $REPO_ROOT/tests/lima/pnfs/rig-writer.py \
           $MNT/a.bin /var/tmp/rig-writer.done 2000000 >/var/tmp/rig-writer.err 2>&1 &
         echo \$! > /var/tmp/rig-writer.pid"
  WA1=$(lvol_written); sleep 3; WA2=$(lvol_written)
  [ "${WA2:-0}" -gt "${WA1:-0}" ] || fail "host A's writer never reached the device ($WA1 -> $WA2)"
  CONFLICT0=$(vsh "grep -c 'range held by clients' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  set +e
  B_SAME=$(vsudo2 "dd if=/dev/urandom of=$MNTB/a.bin bs=1M count=2 oflag=direct conv=notrunc status=none 2>&1")
  B_SAME_RC=$?
  set -e
  CONFLICT1=$(vsh "grep -c 'range held by clients' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  if [ "$B_SAME_RC" -ne 0 ]; then
    echo "· host B's write to A's open file was REFUSED (expected): $(echo "$B_SAME" | tr '\n' ' ' | cut -c1-90)"
  else
    echo "· host B's write to A's open file SUCCEEDED — the MDS granted it non-overlapping space"
  fi
  [ "${CONFLICT1:-0}" -ge "${CONFLICT0:-0}" ] || fail "conflict counter went backwards"
  OVERLAP2=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM extents e1 JOIN extents e2
        ON e1.volume = e2.volume AND e1.rowid <> e2.rowid
       WHERE e1.volume='$VOL'
         AND e1.physical_offset < e2.physical_offset + e2.length
         AND e2.physical_offset < e1.physical_offset + e1.length\"")
  [ "${OVERLAP2:-1}" = "0" ] \
    || fail "same-file contention produced $OVERLAP2 overlapping extent pair(s) — GrantsExclusive VIOLATED"
  echo "✓ M3: same-file contention left the extent map disjoint (${CONFLICT1} conflict refusal(s) logged)"

  # M4. THE PER-CLIENT FENCE. Name host B's NFS client and fence it.
  CID_B=$(client_id_of_nqn "$HOSTNQN2")
  [ -n "$CID_B" ] || fail "no admitted client id for host B's NQN $HOSTNQN2"
  CID_A=$(client_id_of_nqn "$HOSTNQN_A")
  [ -n "$CID_A" ] || fail "no admitted client id for host A's NQN $HOSTNQN_A"
  [ "$CID_A" != "$CID_B" ] || fail "both hosts resolved to client $CID_A — the admission is not per-host"
  # Give B something live to lose: a held-open writer on a SCRATCH file
  # of its own. Not b.bin — the writer rewrites its target with zeros,
  # and b.bin is the sha-checked evidence that B's COMMITTED bytes
  # survive the fence (run 1 failed on exactly that self-inflicted
  # mismatch).
  vsudo2 "rm -f /var/tmp/rig-writer.done
          nohup python3 $REPO_ROOT/tests/lima/pnfs/rig-writer.py \
            $MNTB/bw.bin /var/tmp/rig-writer.done 2000000 >/var/tmp/rig-writer.err 2>&1 &
          echo \$! > /var/tmp/rig-writer.pid"
  sleep 3
  FR=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID_B\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/FenceBlockClient") \
    || fail "FenceBlockClient RPC failed"
  echo "$FR" | grep -c '"fenced": true' >/dev/null || fail "fence lever refused: $FR"

  # B is out: its NQN leaves the allow-list, A's stays.
  HOSTS2=$(vsh "$RPC nvmf_get_subsystems" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(' '.join(h['nqn'] for h in s.get('hosts', []))); break
")
  echo "$HOSTS2" | grep -q "$HOSTNQN2" \
    && fail "fenced host B is STILL on the allow-list: $HOSTS2"
  echo "$HOSTS2" | grep -q "$HOSTNQN_A" \
    || fail "fencing B evicted A too — the eviction is not per-host: $HOSTS2"
  echo "✓ M4a: fencing B evicted ONLY B's NQN; A's admission survived"

  # M4a'. The roller must see the fence too, and see it the RIGHT way
  # round: a fenced client is already cut off at the device, so counting
  # it would refuse this node's roll on behalf of a client we
  # deliberately evicted — while A, still live, must keep the refusal
  # standing on its own.
  BSF=$(vsh "$CSI_CLI block-status --endpoint 127.0.0.1:50051") || fail "M4a' block-status"
  [ "$(echo "$BSF" | grep -c "nqn=$HOSTNQN2" || true)" = "0" ] \
    || fail "block-status still counts the FENCED host B as an initiator: $BSF"
  [ "$(echo "$BSF" | grep -c "nqn=$HOSTNQN_A" || true)" -ge 1 ] \
    || fail "block-status lost the still-live host A after B's fence: $BSF"
  echo "✓ M4a': the roller's read drops the fenced host and keeps the live one"

  # M4b. THE FENCE ITSELF. Eviction is bookkeeping — it closes the
  # RECONNECT door. Delivery is the victim's live raw I/O stopping at
  # the device, and with a second healthy host on the same namespace
  # that is a per-client claim no single-client drill could make: the
  # preempt takes B's key so EA-RO excludes B, while A (a registrant)
  # writes on.
  BDONE=""
  for i in $(seq 1 60); do
    BDONE=$(vsh2 "cat /var/tmp/rig-writer.done 2>/dev/null" || true)
    [ -n "$BDONE" ] && break
    sleep 0.5
  done
  [ -n "$BDONE" ] \
    || fail "host B's raw writer NEVER stopped after its fence — FenceReaches failed for the fenced client (err: $(vsh2 'tail -2 /var/tmp/rig-writer.err 2>/dev/null'))"
  echo "✓ M4b: the fenced host's raw writer stopped at the device ($BDONE)"

  # …and the open question: does the volume-wide reservation stop A's
  # raw I/O as collateral? Both answers are asserted.
  sleep 3
  FA1=$(lvol_written); sleep 3; FA2=$(lvol_written)
  if [ "${FA2:-0}" -gt "${FA1:-0}" ]; then
    COLLATERAL=no
    echo "✓ M4c: host A kept writing raw THROUGH B's fence ($FA1 -> $FA2) — the"
    echo "  reservation admits A (its kernel registered), so the fence is"
    echo "  per-client end to end, at the device as well as the allow-list"
  else
    COLLATERAL=yes
    echo "· M4c FINDING: host A's raw I/O ALSO stopped ($FA1 = $FA2). The"
    echo "  EA-RO reservation is volume-wide and A is not a registrant, so a"
    echo "  per-client fence is collateral at the device. A must recover when"
    echo "  the fence lifts — asserted next."
  fi

  # Unfence and require A healthy either way.
  UR=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID_B\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/UnfenceBlockClient") \
    || fail "UnfenceBlockClient RPC failed"
  echo "$UR" | grep -c '"unfenced": true' >/dev/null || fail "unfence refused: $UR"
  vsudo "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null; true"
  sleep 2
  UA1=$(lvol_written)
  vsudo "dd if=/dev/urandom of=$MNT/a2.bin bs=1M count=2 oflag=direct conv=fsync status=none" \
    || fail "host A cannot write after B's fence was lifted (collateral=$COLLATERAL)"
  UA2=$(lvol_written)
  [ "${UA2:-0}" -gt "${UA1:-0}" ] \
    || fail "host A's post-unfence write never reached the device ($UA1 -> $UA2)"
  vsudo "echo 3 > /proc/sys/vm/drop_caches"
  SHA_B_AFTER=$(vsudo "sha256sum $MNT/b.bin" | awk '{print $1}')
  [ "$SHA_B_AFTER" = "$SHA_B" ] \
    || fail "host B's data changed across the fence: $SHA_B_AFTER != $SHA_B"
  echo "✓ M4d: after unfence host A writes raw again and B's committed bytes are intact"

  # Teardown of host B (production verbs), then prove A is untouched.
  vsudo2 "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null; true"
  vsudo2 "umount -lf $MNTB 2>/dev/null; true"
  vsudo2 "$CSI_CLI unstage --volume-id $VOL" >/dev/null 2>&1 || true
  vsh2 "$CSI_CLI detach --endpoint host.lima.internal:$PXY_GRPC --volume-id $VOL --node \$(hostname)" \
    >/dev/null || fail "host B detach failed"
  ROWS_AFTER=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL' AND host_nqn='$HOSTNQN2'\"")
  [ "${ROWS_AFTER:-1}" = "0" ] || fail "host B's attach row survived its detach"
  vsudo "dd if=/dev/urandom of=$MNT/a3.bin bs=1M count=2 oflag=direct conv=fsync status=none" \
    || fail "host A broke when host B detached"
  echo "✓ M5: host B detached cleanly; host A unaffected"

  [ -f "$PXY_PID_FILE" ] && { kill "$(cat $PXY_PID_FILE)" 2>/dev/null || true; rm -f "$PXY_PID_FILE"; }
  echo
  echo "✅ multi-node rig PASSED — two REAL client hosts on one block volume:"
  echo "   both admitted at once, both writing raw with a physically DISJOINT"
  echo "   extent map, same-file contention refused rather than overlapped, and"
  echo "   a fence that named ONE client evicted only that client (device-side"
  echo "   collateral: $COLLATERAL) with the survivor healthy across it."
  exit 0
fi

# ── E. EXPAND=1: capacity is real — ENOSPC, then a live expand ───────
# The block class owns real capacity in TWO places (the lvol behind the
# export and the allocator's arena ceiling), and before this drill CSI
# expand moved NEITHER: the PVC reported the new size while every
# LAYOUTGET past the old ceiling answered NoSpace forever. Two
# properties, one flow, on its own small volume:
#   E1. a FULL arena reports ENOSPC to the application — not EIO. The
#       app must be able to tell "disk full" from "I/O error"; the
#       fallback lane's blanket EIO said the wrong one.
#   E2. ExpandVolume grows the lvol, the KERNEL picks the bigger
#       namespace up (SPDK's resize AEN → nvme rescan — the one link in
#       the chain no unit test can stand in for), the ceiling rises, and
#       the same mount writes past the old ceiling with the bytes intact.
#
# MDS_BOUNCE=1 — restart the MDS between the device fetch and the expand,
# then expand. The drill that found the bug and now guards the fix
# (measured 2026-08-12, both arms in one session):
#   before: 0 notifications, and the write failed with EIO — 52
#           zeros-belt refusals as the client fell back to the MDS lane
#           on a volume that had the space.
#   after:  1 notification accepted, same mount wrote past the old
#           ceiling sha-intact, exactly like the un-bounced control.
# The MDS log says why the fix had to be keyed on the CLIENT: startup
# logs "observed N persisted sessions ... dropped them so kernel
# re-CREATE_SESSIONs naturally on BADSESSION", the client then does
# CREATE_SESSION with a NEW session id under its EXISTING clientid (no
# EXCHANGE_ID at all), and issues NO fresh GETDEVICEINFO — so its cached
# blocklayout device outlives the session that fetched it. A durable
# book keyed on SessionId would have restored dead addresses; the client
# id is what both sides still agree on, and the session is resolved at
# send time from live state.
if [ "${EXPAND:-0}" = "1" ]; then
  # Derived, not spelled out: cleanup() tears these down by the same
  # "-e" suffix rule, and two independent spellings would drift.
  VOLE="$VOL-e"
  MNTE="$MNT-e"
  SUBNQNE="$SUBNQN-e"
  E_START=$((32 * 1024 * 1024))
  E_GROWN=$((128 * 1024 * 1024))

  # E0. a deliberately SMALL volume — filling 512 MiB to prove ENOSPC
  # would be a throughput test wearing a capacity test's clothes.
  CVE=$(vsh "$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOLE\",\"sizeBytes\":$E_START,\"layoutClass\":\"scsi\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/CreateVolume") || fail "CreateVolume ($VOLE)"
  echo "$CVE" | grep -c '"created": true' >/dev/null || fail "CreateVolume ($VOLE) refused: $CVE"
  STAGEE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
          $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOLE --node \$(hostname)") \
    || fail "pnfs-csi-cli stage ($VOLE)"
  NSDEVE=$(basename "$(echo "$STAGEE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)")
  [ -n "$NSDEVE" ] || fail "stage ($VOLE) reported no device: $STAGEE"
  vsudo "mkdir -p $MNTE && mount -t nfs4 -o vers=4.2,proto=tcp,port=20490 127.0.0.1:/$VOLE $MNTE" \
    || fail "mount ($VOLE)"
  # The kernel's view of the namespace, in 512-byte sectors — this is
  # the number the resize AEN has to move, and the one that gates
  # whether a bio past the old end is even submittable.
  SECT0=$(vsh "cat /sys/block/$NSDEVE/size")
  CEIL0=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT size_ceiling FROM volume_alloc WHERE volume='$VOLE'\"")
  [ "${CEIL0:-0}" = "$E_START" ] || fail "arena ceiling is '${CEIL0:-none}', expected $E_START"
  echo "✓ $VOLE staged+mounted: /dev/$NSDEVE, $SECT0 sectors, ceiling ${CEIL0}B"

  # E1. FILL IT. O_DIRECT so the error surfaces to dd rather than being
  # deferred into writeback where nobody is listening.
  # `|| true` INSIDE the remote command: grep -c exits 1 on zero matches,
  # and a host-side `|| echo 0` appends a SECOND line to grep's own "0" —
  # the multi-line value then breaks the -gt comparison below (run 1).
  BELT_PRE=$(vsh "grep -c 'EXHAUSTED extent arena' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  set +e
  FILL_ERR=$(vsudo "dd if=/dev/zero of=$MNTE/fill.bin bs=1M count=64 oflag=direct conv=fsync 2>&1 >/dev/null")
  set -e
  echo "· fill result: $(echo "$FILL_ERR" | tr '\n' ' ' | cut -c1-200)"
  echo "$FILL_ERR" | grep -qi 'No space left on device' \
    || fail "filling a $((E_START / 1024 / 1024))MiB volume did NOT report ENOSPC — got: $FILL_ERR"
  BELT_POST=$(vsh "grep -c 'EXHAUSTED extent arena' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  [ "${BELT_POST:-0}" -gt "${BELT_PRE:-0}" ] \
    || fail "the app saw ENOSPC but the MDS never logged an exhausted arena — \
wrong lane? ($BELT_PRE → $BELT_POST)"
  USED=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT next_free FROM volume_alloc WHERE volume='$VOLE'\"")
  echo "✓ E1: the app got ENOSPC (not EIO) with the arena at ${USED}B of ${CEIL0}B"

  # E1b. MDS_BOUNCE=1 — THE EXPERIMENT (residual: the device-notify
  # address book is in-memory).
  #
  # `LayoutManager.device_notify` is a DashMap: which sessions fetched
  # which volume's device, and what notify mask they asked for. It does
  # not survive a restart. The open question is whether that MATTERS,
  # and it is not answerable by reading either side alone:
  #
  #   * flint PERSISTS sessions, so an MDS restart can be transparent to
  #     the client — no EXCHANGE_ID, no state purge, and (the part that
  #     bites) no reason for it to drop its cached blocklayout device.
  #   * but if the client re-establishes and re-fetches anyway, the book
  #     repopulates itself and there is nothing to fix.
  #
  # So: bounce the MDS here, with the client mounted and its device
  # already cached, let it fully reconnect, and then let E2/E2c judge.
  # The verdict is E2c's SAME-MOUNT write, not the notification count —
  # a client that self-heals without a callback is a pass.
  if [ "${MDS_BOUNCE:-0}" = "1" ]; then
    MDS_PID0=$(vsh "pgrep -x flint-pnfs-mds | head -1" || true)
    [ -n "$MDS_PID0" ] || fail "no MDS process to bounce"
    vsudo "pkill -9 -x flint-pnfs-mds"
    for i in $(seq 1 20); do
      vsh "pgrep -x flint-pnfs-mds" >/dev/null || break
      [ "$i" = 20 ] && fail "MDS never exited"
      sleep 0.5
    done
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
    for i in $(seq 1 20); do
      vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
      [ "$i" = 20 ] && fail "MDS gRPC never came back after the bounce"
      sleep 0.5
    done
    MDS_PID1=$(vsh "pgrep -x flint-pnfs-mds | head -1" || true)
    [ -n "$MDS_PID1" ] && [ "$MDS_PID1" != "$MDS_PID0" ] \
      || fail "MDS pid did not change ($MDS_PID0 → $MDS_PID1) — nothing was bounced"
    # Out of the grace window, then drive REAL client traffic so the
    # session is re-established and any device re-fetch it would do on
    # its own has already happened BEFORE the expand. Without this the
    # post-expand write would confound "the notification reached it"
    # with "reconnecting refreshed it".
    sleep 8
    vsudo "stat $MNTE >/dev/null" || fail "the mount did not survive the MDS bounce"
    vsudo "dd if=$MNTE/fill.bin of=/dev/null bs=1M count=1 iflag=direct status=none" \
      || fail "post-bounce read failed — the client never recovered its session"
    FETCH_AFTER=$(vsh "grep -c 'GETDEVICEINFO (scsi)' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
    echo "· E1b: MDS bounced ($MDS_PID0 → $MDS_PID1), client reconnected and read;"
    echo "       cumulative GETDEVICEINFO count now ${FETCH_AFTER:-?} (the address book"
    echo "       is repopulated ONLY by a fetch after the restart)"
  fi

  # E2. THE EXPAND. Device first, then ceiling — the ordering the MDS
  # enforces; here we only check both landed.
  EXR=$(vsh "$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOLE\",\"sizeBytes\":$E_GROWN}' \
        127.0.0.1:50051 pnfs.control.MdsControl/ExpandVolume") || fail "ExpandVolume RPC"
  echo "$EXR" | grep -c '"expanded": true' >/dev/null || fail "ExpandVolume refused: $EXR"
  CEIL1=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT size_ceiling FROM volume_alloc WHERE volume='$VOLE'\"")
  [ "${CEIL1:-0}" = "$E_GROWN" ] || fail "ceiling did not rise: '${CEIL1:-none}' != $E_GROWN"
  DEVB=$(vsh "$RPC bdev_get_bdevs --name lvs_rig/$VOLE" | python3 -c "
import json,sys
b = json.load(sys.stdin)[0]
print(b['block_size'] * b['num_blocks'])")
  [ "${DEVB:-0}" -ge "$E_GROWN" ] || fail "lvol is ${DEVB}B, below the new ceiling $E_GROWN"

  # THE LINK NO UNIT TEST COVERS: does the client's kernel actually pick
  # the resize up? SPDK turns the bdev resize into nvmf_ns_resize and
  # sends the ns-changed AEN; the kernel is supposed to rescan. If it
  # does not, every write past the old end fails at the block layer and
  # the expand is cosmetic.
  SECT1=$SECT0
  for i in $(seq 1 30); do
    SECT1=$(vsh "cat /sys/block/$NSDEVE/size")
    [ "${SECT1:-0}" -gt "${SECT0:-0}" ] && break
    sleep 1
  done
  [ "${SECT1:-0}" -gt "${SECT0:-0}" ] \
    || fail "the client kernel still sees $SECT1 sectors after the resize — the AEN/rescan never landed"
  [ $((SECT1 * 512)) -ge "$E_GROWN" ] \
    || fail "kernel sees $((SECT1 * 512))B, below the new ceiling $E_GROWN"
  echo "✓ E2a: lvol ${DEVB}B, ceiling ${CEIL1}B, kernel $SECT0 → $SECT1 sectors (resize AEN landed)"

  # E2b. and the volume WORKS past the old ceiling: fresh bytes, raw
  # path, sha-checked cold.
  #
  # ON THE SAME MOUNT — the property CB_NOTIFY_DEVICEID exists for.
  #
  # Rig runs 3-4 (before the notification shipped) found this failing:
  # the server granted layouts past the old ceiling, the client returned
  # each one instantly and wrote through the MDS lane, for EVERY new
  # file on that mount, because it caches the blocklayout device — its
  # LENGTH included — from GETDEVICEINFO and nothing told it to re-read.
  # Only recycling the mount recovered. Now the expand sends
  # CB_NOTIFY_DEVICEID to every client that fetched the device, Linux's
  # nfs4_callback_devicenotify drops the cached deviceid, and the next
  # LAYOUTGET re-fetches it. NO REMOUNT: that is the whole point, so the
  # drill fails rather than falling back to one.
  vsh "dd if=/dev/urandom of=/var/tmp/rig-e.bin bs=1M count=16 status=none"
  E_SHA=$(vsh "sha256sum /var/tmp/rig-e.bin" | awk '{print $1}')
  NOTIFIED=$(vsh "grep -c 'CB_NOTIFY_DEVICEID ← .*accepted' $RIG/mds.log || true" | head -1 | tr -dc '0-9')
  [ "${NOTIFIED:-0}" -ge 1 ] \
    || fail "no client ACCEPTED a CB_NOTIFY_DEVICEID${MDS_BOUNCE:+ (after the MDS bounce — the durable notify book did not survive, or the client could not be reached through its NEW session)} — $(vsh "grep -i 'notify_deviceid' $RIG/mds.log | tail -3")"
  set +e
  SAME_ERR=$(vsudo "cp /var/tmp/rig-e.bin $MNTE/after-expand.bin 2>&1 >/dev/null")
  SAME_RC=$?
  set -e
  [ "$SAME_RC" -eq 0 ] || fail "the SAME mount still cannot use the new room after \
CB_NOTIFY_DEVICEID ($(echo "$SAME_ERR" | tr '\n' ' ' | cut -c1-110)) — the client kept \
its cached device"
  vsudo "sync $MNTE/after-expand.bin" || fail "sync after expand"
  echo "✓ E2c: $NOTIFIED client(s) took CB_NOTIFY_DEVICEID; the SAME mount wrote past"
  echo "  the old ceiling with no remount"
  NEXT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT next_free FROM volume_alloc WHERE volume='$VOLE'\"")
  [ "${NEXT:-0}" -gt "${CEIL0:-0}" ] \
    || fail "the post-expand write allocated nothing past the OLD ceiling ($NEXT ≤ $CEIL0)"
  vsudo "echo 3 > /proc/sys/vm/drop_caches"
  E_SHA2=$(vsudo "sha256sum $MNTE/after-expand.bin" | awk '{print $1}')
  [ "$E_SHA" = "$E_SHA2" ] || fail "post-expand sha mismatch: $E_SHA != $E_SHA2"
  echo "✓ E2b: 16MiB written past the old ceiling (watermark $NEXT > $CEIL0), cold-read intact"

  # Teardown of the drill's own volume; the base flow owns $VOL.
  #
  # QUIESCE FIRST, and this is not politeness: pulling the nvme session
  # while the client still has writeback and a LAYOUTRETURN/LAYOUTCOMMIT
  # in flight parks nvme-wq kworkers in D — a VM reboot is the only exit
  # (rig-relearned here the day LAYOUTCOMMIT started SUCCEEDING, which
  # gave the umount real work to do where before it gave up). Same
  # lesson as the PREEMPT reap, different door.
  vsudo "sync $MNTE 2>/dev/null; umount $MNTE" >/dev/null 2>&1 || true
  for i in $(seq 1 40); do
    LEFT_E=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
      \"SELECT COUNT(*) FROM extent_grants WHERE volume='$VOLE'\"" 2>/dev/null || echo "?")
    [ "${LEFT_E:-1}" = "0" ] && break
    sleep 0.5
  done
  [ "${LEFT_E:-1}" = "0" ] \
    || echo "· note: $VOLE still shows ${LEFT_E} grant row(s) at teardown — leaving the session up"
  if [ "${LEFT_E:-1}" = "0" ]; then
    vsudo "$CSI_CLI unstage --volume-id $VOLE" >/dev/null || true
    vsh "$CSI_CLI detach --endpoint 127.0.0.1:50051 --volume-id $VOLE --node \$(hostname)" >/dev/null || true
    vsudo "nvme disconnect -n $SUBNQNE 2>/dev/null; true"
  fi

  echo
  echo "✅ expand-rig PASSED — a full block volume reported ENOSPC to the app"
  echo "   (not EIO), and a live ExpandVolume grew the lvol, moved the KERNEL's"
  echo "   namespace size ($SECT0 → $SECT1 sectors), raised the arena ceiling, and"
  echo "   CB_NOTIFY_DEVICEID dropped the client's cached device so the SAME MOUNT"
  echo "   wrote past the old ceiling sha-intact — no remount. On $KREL."
  exit 0
fi

# ── X. PREEMPT=1: the foreign-holder fence arm + per-namespace scope ─
# The one fence branch no drill ever fired: every fence so far found an
# UNHELD reservation (a conforming kernel registers no key), so
# fence_preempt's foreign-holder arm — the intruder / stale-MDS shape —
# has only unit tests. Here the rig client turns adversary: it
# REGISTERS its own key and ACQUIRES Write Exclusive on volume A. The
# fence must preempt the holder, take EA-RO, wipe the key, and freeze
# the writer — while volume B, staged by the SAME host on the SAME tgt,
# never notices (the reservation is per-namespace, the eviction
# per-subsystem).
if [ "${PREEMPT:-0}" = "1" ]; then
  # X0. the scope-control volume. No NFS mount: the properties under
  # test are NVMe-level, so raw device I/O through its own staged
  # session is the honest probe.
  VOLB=rigvol-b
  CVB=$(vsh "$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOLB\",\"sizeBytes\":$((128 * 1024 * 1024)),\"layoutClass\":\"scsi\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/CreateVolume") || fail "CreateVolume ($VOLB)"
  echo "$CVB" | grep -c '"created": true' >/dev/null || fail "CreateVolume ($VOLB) refused: $CVB"
  STAGEB=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
          $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOLB --node \$(hostname)") \
    || fail "pnfs-csi-cli stage ($VOLB)"
  NSDEVB=$(basename "$(echo "$STAGEB" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)")
  [ -n "$NSDEVB" ] || fail "stage ($VOLB) reported no device: $STAGEB"
  lvol_written_b() {
    vsh "$RPC bdev_get_iostat --name lvs_rig/$VOLB" | python3 -c "
import json,sys; print(json.load(sys.stdin)['bdevs'][0]['bytes_written'])"
  }
  B0=$(lvol_written_b)
  vsudo "dd if=/dev/zero of=/dev/$NSDEVB bs=1M count=4 oflag=direct conv=fsync 2>/dev/null" \
    || fail "pre-fence raw write to $VOLB failed"
  B1=$(lvol_written_b)
  [ "${B1:-0}" -gt "${B0:-0}" ] || fail "control volume $VOLB counter never moved ($B0 → $B1)"
  echo "✓ control volume $VOLB staged (/dev/$NSDEVB): ${B1}B written raw"

  # X1. the victim writer on A (the held-open F0/F1 shape).
  vsudo "rm -f /var/tmp/rig-writer.done
         nohup python3 $REPO_ROOT/tests/lima/pnfs/rig-writer.py \
           $MNT/data.bin /var/tmp/rig-writer.done >/var/tmp/rig-writer.err 2>&1 &
         echo \$! > /var/tmp/rig-writer.pid"
  W1=$(lvol_written); sleep 3; W2=$(lvol_written)
  [ "${W2:-0}" -gt "${W1:-0}" ] || fail "writer never reached the device ($W1 → $W2)"
  echo "✓ live raw-path writer on $VOL: bytes_written $W1 → $W2 and climbing"

  # X2. THE ADVERSARY: take the host's registration and acquire Write
  # Exclusive. REPLACE+IEKEY, not plain REGISTER — drill-found: THIS
  # kernel REGISTERS its GETDEVICEINFO pr_key (SPDK refused the plain
  # register with "already register a key with 0x2" = the client id),
  # refuting the fence-era "kernel registers no key" observation, on
  # which kernels a plain register conflicts. Replace-with-iekey covers
  # both worlds: existing registrant (any key) → key swapped, none →
  # fresh registrant. The writer (same host = the holder) must sail on —
  # a zombie WITH a reservation, the strongest adversary this layer can
  # host.
  IKEY=0xdeadbeef
  vsudo "nvme resv-register /dev/$NSDEV --rrega=2 --iekey --nrkey=$IKEY" \
    || fail "intruder resv-register (replace+iekey) refused"
  vsudo "nvme resv-acquire /dev/$NSDEV --crkey=$IKEY --rtype=1 --racqa=0" \
    || fail "intruder resv-acquire refused"
  RESV_I=$(vsudo "nvme resv-report /dev/$NSDEV -c 1 -e 2>/dev/null | grep -iE 'rtype|regctl|rkey' | tr '\n' ' '" || true)
  echo "· intruder holds the reservation: ${RESV_I:-<report unavailable>}"
  W2b=$(lvol_written); sleep 2; W2c=$(lvol_written)
  [ "${W2c:-0}" -gt "${W2b:-0}" ] \
    || fail "the holder's own writer stopped ($W2b → $W2c) — WE must not block the holder"
  echo "✓ writer still climbing UNDER the intruder's WE reservation ($W2b → $W2c)"

  # X3. the fence. Same lever as F2; what is NEW is what it must find.
  CID=$(vsh "grep -oE 'client [0-9]+' $RIG/mds.log | grep -oE '[0-9]+' | tail -1" || true)
  [ -n "$CID" ] || CID=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT DISTINCT client_id FROM extent_grants WHERE volume='$VOL' LIMIT 1\"")
  [ -n "$CID" ] || fail "no scsi client id to fence"
  FR=$(vsh "timeout 45 $RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto \
        -d '{\"volumeId\":\"$VOL\",\"clientId\":\"$CID\"}' \
        127.0.0.1:50051 pnfs.control.MdsControl/FenceBlockClient") \
    || fail "FenceBlockClient RPC — breadcrumbs: $(vsh "grep -iE 'fence_preempt|resv fence' $RIG/mds.log | tail -3")"
  echo "$FR" | grep -c '"fenced": true' >/dev/null || fail "fence lever refused: $FR"
  # THE BRANCH PROOF: the MDS's own pre-preempt report named the foreign
  # holder — fence_preempt's warn line, verbatim key and all.
  vsh "grep -q 'foreign reservation holder $IKEY' $RIG/mds.log" \
    || fail "the foreign-holder preempt arm never fired — fence log: $(vsh "grep -iE 'resv fence' $RIG/mds.log | tail -3")"
  RESV_LINE=$(vsh "grep -o 'resv: .*' $RIG/mds.log | tail -1" || true)
  echo "$RESV_LINE" | grep -q 'rtype=0x4' \
    || fail "MDS does not hold EA-RO after the preempt — resv: ${RESV_LINE:-<none>}"
  echo "$RESV_LINE" | grep -qi 'deadbeef' \
    && fail "the intruder key SURVIVED the preempt — resv: $RESV_LINE"
  echo "✓ foreign holder preempted; MDS holds EA-RO, intruder key wiped: $RESV_LINE"

  # X4. FenceReaches through the preempt path: A's bytes stop.
  sleep 3
  W3=$(lvol_written); sleep 3; W4=$(lvol_written)
  [ "${W3:-0}" = "${W4:-1}" ] \
    || fail "bytes_written STILL CLIMBING after the preempt ($W3 → $W4)"
  echo "✓ FenceReaches via preempt: bytes_written frozen at $W4"

  # X5. and B never noticed: the SAME host still writes it raw. A leak
  # here would mean the fence's reservation or eviction escaped its
  # namespace/subsystem — the multi-volume deployment killer.
  vsudo "dd if=/dev/zero of=/dev/$NSDEVB bs=1M count=4 oflag=direct conv=fsync 2>/dev/null" \
    || fail "post-fence raw write to $VOLB FAILED — the fence leaked across namespaces"
  B2=$(lvol_written_b)
  [ "${B2:-0}" -gt "${B1:-0}" ] || fail "control counter frozen ($B1 → $B2) — the fence leaked to $VOLB"
  RESV_B=$(vsudo "nvme resv-report /dev/$NSDEVB -c 1 -e 2>/dev/null | grep -iE 'rtype|regctl' | tr '\n' ' '" || true)
  echo "✓ per-namespace scope: $VOLB still writes raw ($B1 → ${B2}B); its resv: ${RESV_B:-<none>}"

  # Teardown, wedge-aware. B FIRST (healthy — bank its production
  # teardown while nothing can wedge it). Then WAIT for the fenced
  # writer to surface its error (the F4 shape): disconnecting while its
  # O_DIRECT pwrite is still in flight is the D-state landmine —
  # rig-relearned HERE (run 2 wedged nvme-delete-wq + the writer in D;
  # VM reboot was the only exit). If the writer never exits, SKIP the
  # disconnect: a leaked controller costs the next run nothing (its
  # cleanup reboots state), a wedged nvme-delete-wq costs a reboot NOW.
  vsudo "$CSI_CLI unstage --volume-id $VOLB" >/dev/null || true
  vsh "$CSI_CLI detach --endpoint 127.0.0.1:50051 --volume-id $VOLB --node \$(hostname)" >/dev/null || true
  DONE=""
  for i in $(seq 1 40); do
    DONE=$(vsh "cat /var/tmp/rig-writer.done 2>/dev/null" || true)
    [ -n "$DONE" ] && break
    sleep 0.5
  done
  if [ -n "$DONE" ]; then
    vsudo "[ -f /var/tmp/rig-writer.pid ] && kill -9 \$(cat /var/tmp/rig-writer.pid) 2>/dev/null; true"
    vsudo "nvme disconnect -n $SUBNQN 2>/dev/null; true"
    echo "· writer surfaced its error ($DONE); A disconnected"
  else
    echo "· writer still blocked in its fenced pwrite — leaving A connected (D-state belt)"
  fi

  echo
  echo "✅ preempt-rig PASSED — an adversarial holder (registered key + WE"
  echo "   reservation: the intruder/stale-MDS shape) was PREEMPTED by the fence."
  echo "   The MDS took EA-RO over it, wiped its key, froze its writer — and the"
  echo "   same host's OTHER volume never noticed. On $KREL."
  exit 0
fi

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
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
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
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
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
    vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
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

# ── P. SWEEP=1: the lease-sweep partition drill ──────────────────────
# The DANGEROUS partition shape: the NFS port dies (leases stop
# renewing) while the raw NVMe path stays ALIVE — a zombie writer the
# lease sweep must fence, revoke, and auto-unfence with NO operator
# lever anywhere. Then the node reboots (the D-state landmine's only
# reliable clearance) and a successor stages, mounts, and writes —
# the RWO recovery story, end to end on the timer.
if [ "${SWEEP:-0}" = "1" ]; then
  # P0. a held-open raw-path writer (the F0 shape: the held fd keeps
  # the grant row live; pure pwrites, no fsync — nothing it does needs
  # the NFS port once the layout is granted). Cap raised to outlive the
  # 90s lease against the page-cache-fast lvol (~3 GB/s here).
  vsudo "rm -f /var/tmp/rig-writer.done
         nohup python3 $REPO_ROOT/tests/lima/pnfs/rig-writer.py \
           $MNT/data.bin /var/tmp/rig-writer.done 2000000 >/var/tmp/rig-writer.err 2>&1 &
         echo \$! > /var/tmp/rig-writer.pid"
  W0=$(lvol_written); sleep 3; W1=$(lvol_written)
  [ "${W1:-0}" -gt "${W0:-0}" ] || fail "writer never reached the device ($W0 → $W1)"
  echo "✓ live raw-path writer: bytes_written $W0 → $W1 and climbing"

  # P1. partition the NFS port ONLY. 4420 stays open.
  vsudo "iptables -A OUTPUT -p tcp --dport 20490 -j DROP"
  echo "✓ NFS port 20490 partitioned (lease renewals now black-holed; nvme 4420 open)"

  # P2. the zombie WINDOW — measured, not asserted. RIG-FOUND (two
  # drafts refuted by the client): a LIVE partitioned kernel freezes
  # its raw block-path I/O near-INSTANTLY — the O_DIRECT write path is
  # coupled to the metadata lane closely enough (per-write attribute /
  # commit RPC suspected, cheap on a healthy lane at 3 GB/s, blocking
  # the moment the lane dies) that a network partition cannot
  # manufacture a data-plane zombie at all. STRONGER client behaviour
  # than 'honors lease expiry' — and exactly why the same-node-zombie
  # model (FlintAdmissionZombie.cfg) uses the frozen-VM shape, which
  # skips every client courtesy. The sweep's necessity is untouched:
  # nothing client-side frees the MDS's rows.
  sleep 2
  W2=$(lvol_written); sleep 5; W3=$(lvol_written)
  if [ "${W3:-0}" -gt "${W2:-0}" ]; then
    echo "· zombie window OPEN at +7s ($W2 → $W3) — this kernel kept writing"
  else
    echo "· zombie window ~0s (raw I/O froze with the partition — client-side coupling)"
  fi

  # P3. expiry + sweep: within lease(90s)+sweep(5s)+margin the sweep
  # must fence the dead client, bulk-revoke its rows, and auto-unfence.
  SWEPT=""
  for i in $(seq 1 40); do
    SWEPT=$(vsh "grep -c 'lease sweep: ' $RIG/mds.log" || true)
    [ "${SWEPT:-0}" -ge 1 ] && break
    sleep 5
  done
  [ "${SWEPT:-0}" -ge 1 ] || fail "the lease sweep never swept (waited ~200s past the partition)"
  GRANTS_LEFT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM extent_grants WHERE volume='$VOL'\"" || echo "?")
  [ "${GRANTS_LEFT:-1}" = "0" ] || fail "sweep left $GRANTS_LEFT grant row(s)"
  FCOUNT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM fenced_clients WHERE volume='$VOL'\"" || echo "?")
  [ "${FCOUNT:-1}" = "0" ] || fail "auto-unfence left $FCOUNT fenced_clients row(s)"
  HOSTROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_hosts WHERE volume='$VOL'\"" || echo "?")
  [ "${HOSTROWS:-1}" = "0" ] || fail "the dead client's admission survived the sweep ($HOSTROWS row(s))"
  NAROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"" || echo "?")
  [ "${NAROWS:-1}" = "0" ] || fail "the dead node's attach row survived the sweep ($NAROWS row(s))"
  REL_LINE=$(vsh "grep -o 'released=.*' $RIG/mds.log | tail -1" || true)
  echo "$REL_LINE" | grep -c 'released=true' >/dev/null \
    || fail "the auto-unfence did not release the reservation — ${REL_LINE:-<no release line>}"
  # The freeze belt: by sweep time the counter must be frozen (the
  # client's own lease expiry and the sweep's fence land near-
  # simultaneously; F-mode owns the causally-clean FenceReaches proof).
  W4=$(lvol_written); sleep 5; W5=$(lvol_written)
  [ "${W4:-0}" = "${W5:-1}" ] || fail "bytes still flowing after the sweep ($W4 → $W5)"
  echo "✓ sweep on the timer: rows revoked, fence auto-released ($REL_LINE), counter frozen at $W5"

  # P4. reboot the node (clears the potential D-state pwrite AND the
  # iptables rule), restart the stack, and assert the startup replay
  # has NOTHING to re-fence — the sweep's auto-unfence left no record.
  echo "· rebooting the node (D-state clearance; iptables heals with it)"
  limactl stop -f "$LIMA_VM" >/dev/null 2>&1; sleep 2
  limactl start "$LIMA_VM" >/dev/null 2>&1 || fail "VM restart failed"
  vsudo "rm -f /var/tmp/spdk_cpu_lock_* $SOCK ${SOCK}.lock"
  vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $SOCK -m 0x1 --wait-for-rpc >>$RIG/spdk.log 2>&1 &
         echo \$! > /var/tmp/spdk-rig.pid; sleep 0.5"
  for i in $(seq 1 20); do
    vsh "$RPC rpc_get_methods >/dev/null 2>&1" && break
    [ "$i" = 20 ] && fail "tgt RPC never came back after the reboot"
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
  vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS FLINT_NFS_GRACE_SECS=5 FLINT_PNFS_LEASE_SWEEP_SECS=$SWEEP_SECS RUST_LOG='${MDS_LOG:-info}' nohup $MDS_BIN --config $CFG >>$RIG/mds.log 2>&1 & sleep 0.5"
  for i in $(seq 1 20); do
    vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/50051' 2>/dev/null" && break
    [ "$i" = 20 ] && fail "MDS gRPC never came back after the reboot"
    sleep 0.5
  done
  REFENCE=$(vsh "grep -c 'startup re-fence' $RIG/mds.log" || true)
  [ "${REFENCE:-0}" = "0" ] \
    || fail "startup re-fenced something — the sweep's auto-unfence left a record behind"
  echo "✓ stack restarted; startup replay found NOTHING to re-fence (clean auto-unfence)"

  # P5. the successor: stage + mount + O_DIRECT write, ZERO levers. The
  # tgt restart zeroed the counters, so everything counted is the
  # successor's.
  vsudo "modprobe nvme-tcp && modprobe blocklayoutdriver"
  STAGE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
          $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
    || fail "successor stage failed"
  vsudo "mkdir -p $MNT && mount -t nfs4 -o vers=4.2,proto=tcp,port=20490 127.0.0.1:/$VOL $MNT" \
    || fail "successor mount failed"
  SUCC_MIB=8
  vsudo "dd if=/dev/urandom of=$MNT/data.bin bs=1M count=$SUCC_MIB \
         oflag=direct conv=notrunc status=none && sync $MNT/data.bin" \
    || fail "successor O_DIRECT write failed"
  W6=$(lvol_written)
  NEED_SW=$((SUCC_MIB * 1024 * 1024))
  [ "${W6:-0}" -ge "$NEED_SW" ] \
    || fail "successor wrote only ${W6}B raw (need ≥$NEED_SW)"
  echo "✓ successor recovered with ZERO operator action: ${W6}B written raw"

  echo
  echo "✅ sweep-rig PASSED — a partitioned client kept writing raw (the zombie the"
  echo "   lazy expiry could never see), the lease sweep fenced it ON THE TIMER,"
  echo "   revoked its rows, auto-released the reservation, and after the reboot a"
  echo "   successor staged and wrote with no lever touched anywhere. On $KREL."
  exit 0
fi

# ── Z. ZOMBIE=1: the frozen-VM zombie — the model's ONLY dangerous shape
# The sweep drill proved a LIVE partitioned kernel freezes its own raw
# I/O instantly, so a partition cannot make a data-plane zombie. What
# CAN is a FROZEN VM (SIGSTOP the hypervisor = live-migration pause):
# its lease dies while its stale extent mappings sleep, the sweep
# fences/revokes/auto-unfences, a successor's fresh grants REUSE the
# freed extents — and then the zombie WAKES. FlintAdmission says the
# cross-host barrier (NQN eviction) must hold, and the same-host
# readmit door is safe only because a live kernel honours its lease.
# This drill runs that exact movie on a second lima VM and asserts the
# one thing that must be true at the device: THE SUCCESSOR'S BYTES
# SURVIVE THE ZOMBIE'S RESUME. Client-side refusal lines are observed
# and reported, not asserted — the resume-side race (nvme reconnect vs
# NFS-lane re-admission) is the kernel's business; the invariant is
# not.
#
# Topology: this VM keeps MDS+tgt and plays the successor; ZOMBIE_VM
# (default flint-zombie: stock 24.04 + HWE kernel + nvme-cli +
# nfs-common) is the client that freezes. lima VMs cannot dial each
# other (isolated user-nets), so the zombie reaches the rig through
# host.lima.internal + tcp-proxy.py on the host + lima's auto-forward
# of this VM's loopback listeners.
if [ "${ZOMBIE:-0}" = "1" ]; then
  ZOMBIE_VM="$VM2"
  MNTZ=/mnt/flint-zombie
  # Thin aliases: Z3-Z8 below were written against these names and are
  # PROVEN, so the shared helpers are wired in rather than renamed
  # through a drill whose value is that it already caught things.
  vshz()  { vsh2 "$@"; }
  vsudoz(){ vsudo2 "$@"; }
  zombie_procs() { vm2_procs; }

  # Z0/Z1. a CLEAN zombie VM + the host proxy (shared with §M).
  vm2_boot
  ZKREL="$VM2_KREL"
  vm2_proxy_up

  # Z2. the zombie stages through the production attach verb + a manual
  # connect to the proxy. Long ctrl-loss: post-resume reconnect attempts
  # are the observable.
  vm2_stage "$VOL" "$MNTZ"
  HOSTNQN_Z="$HOSTNQN2"; NGUID_Z="$NGUID2"; SUBNQN_Z="$SUBNQN2"; NSDEVZ="$NSDEV2"
  [ "$NGUID_Z" = "$NGUID" ] || fail "zombie attach NGUID '$NGUID_Z' != '$NGUID'"

  # Z3. the zombie writer (held-open O_DIRECT — keeps its layout and
  # grant rows live so the freeze captures real stale state). Pre-create
  # the file with a raw O_DIRECT write first: run 3 died on the writer's
  # own missing-file ENOENT, and this doubles as the zombie's first
  # raw-path proof through the proxy.
  ZPRE=$(lvol_written)
  vsudoz "dd if=/dev/zero of=$MNTZ/zdata.bin bs=1M count=2 oflag=direct conv=fsync status=none" \
    || fail "zombie raw pre-create failed"
  ZPOST=$(lvol_written)
  [ "${ZPOST:-0}" -gt "${ZPRE:-0}" ] \
    || fail "zombie pre-create never reached the device ($ZPRE -> $ZPOST) — MDS-path degradation?"
  vsudoz "rm -f /var/tmp/rig-writer.done
          nohup python3 /Users/ddalton/github/flint/tests/lima/pnfs/rig-writer.py \
            $MNTZ/zdata.bin /var/tmp/rig-writer.done 2000000 >/var/tmp/rig-writer.err 2>&1 &
          echo \$! > /var/tmp/rig-writer.pid"
  W1=$(lvol_written); sleep 4; W2=$(lvol_written)
  [ "${W2:-0}" -gt "${W1:-0}" ] \
    || fail "zombie writer never reached the device ($W1 → $W2)"
  ZCID=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT DISTINCT client_id FROM extent_grants WHERE volume='$VOL' LIMIT 1\"")
  [ -n "$ZCID" ] || fail "no zombie grant rows to freeze over"
  ATTACH_PRE=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"")
  echo "✓ zombie writer on the raw path (client $ZCID): $W1 → $W2; $ATTACH_PRE attach row(s) pre-freeze"

  # Z4. FREEZE — SIGSTOP everything that runs the zombie VM (hostagent +
  # its ssh plumbing). The guest's vCPUs stop mid-pwrite; its lease
  # clock, unlike the partition drill's, keeps running EVERYWHERE ELSE.
  kill -STOP $(zombie_procs) 2>/dev/null
  timeout 4 limactl shell "$ZOMBIE_VM" -- true >/dev/null 2>&1 \
    && fail "zombie VM still responsive after SIGSTOP"
  echo "✓ zombie FROZEN mid-write (hypervisor SIGSTOPped)"

  # Z5. the sweep, on the timer, against a frozen client. Its rows
  # revoke, its attach row sweeps, the fence auto-releases — and THIS
  # VM's own attach row (the successor's seat) must SURVIVE the sweep.
  SWEPT=""
  for i in $(seq 1 240); do
    LEFT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
      \"SELECT COUNT(*) FROM extent_grants WHERE volume='$VOL'\"" || echo "?")
    [ "${LEFT:-1}" = "0" ] && { SWEPT=1; break; }
    sleep 1
  done
  [ -n "$SWEPT" ] || fail "the sweep never revoked the frozen client's rows"
  vsh "grep -q 'lease sweep' $RIG/mds.log" || fail "no lease-sweep line in the MDS log"
  vsh "grep -q 'released=true' $RIG/mds.log" || fail "the sweep's fence never auto-released"
  FENCED_LEFT=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' 'SELECT COUNT(*) FROM fenced_clients'")
  [ "${FENCED_LEFT:-1}" = "0" ] || fail "$FENCED_LEFT fenced_clients row(s) after auto-unfence"
  ATTACH_POST=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
    \"SELECT COUNT(*) FROM block_node_attach WHERE volume='$VOL'\"")
  [ "$ATTACH_POST" = "$((ATTACH_PRE - 1))" ] \
    || fail "sweep took attach rows $ATTACH_PRE → $ATTACH_POST (expected exactly the zombie's one)"
  echo "✓ sweep fenced the frozen client on the timer: rows revoked, auto-released, its attach row swept, the successor's kept"

  # Z6. the successor writes THROUGH this VM's own live session — fresh
  # grants at gen+1 over the freed (reused) extents. The pattern's sha
  # is the tripwire the zombie's stale mappings would trip.
  vsudo "dd if=/dev/urandom of=/var/tmp/succ.src bs=1M count=8 status=none"
  SHA_SRC=$(vsudo "sha256sum /var/tmp/succ.src | cut -d' ' -f1")
  vsudo "dd if=/var/tmp/succ.src of=$MNT/successor.bin bs=1M oflag=direct conv=fsync status=none" \
    || fail "successor write failed"
  # Baseline read-back, POLLED: a cold O_DIRECT read races the write's
  # LAYOUTCOMMIT — a range the commit hasn't covered yet falls to the
  # MDS lane, where the scsi zeros-belt answers EIO (run 5 lost exactly
  # this race mid-file; the count-less dd's past-EOF probe hits the same
  # belt every time, harmlessly, after the full 8MiB is out). EIO-not-
  # zeros is the belt WORKING; the poll waits for the committed view.
  # Z8's post-resume assert stays single-shot strict — by then this
  # baseline has proven the data device-readable.
  SHA_1=""
  for i in $(seq 1 20); do
    vsudo "echo 3 > /proc/sys/vm/drop_caches"
    SHA_1=$(vsudo "dd if=$MNT/successor.bin bs=1M iflag=direct status=none 2>/var/tmp/succ.dd.err | sha256sum | cut -d' ' -f1; true")
    [ "$SHA_1" = "$SHA_SRC" ] && break
    sleep 1
  done
  [ "$SHA_1" = "$SHA_SRC" ] \
    || fail "successor data never became device-readable ($SHA_1 != $SHA_SRC) — commit race would have resolved; this is real"
  echo "✓ successor wrote 8MiB over the reused extents (sha $SHA_SRC, stable on poll $i)"
  echo "· read-back dd stderr (belt refusals expected): $(vsh "tr '\n' ' ' < /var/tmp/succ.dd.err")"

  # Z7. RESUME. The zombie wakes into a world where its lease died, its
  # rows were revoked, its NQN was evicted — and then auto-unfence
  # re-opened the door for a FRESH incarnation. Whatever path its kernel
  # takes (refused reconnects, STALE_CLIENTID recovery, a clean
  # re-grant), the successor's bytes must not move. Client-side lines
  # are OBSERVED; the eviction itself was already asserted server-side.
  kill -CONT $(zombie_procs) 2>/dev/null
  sleep 8
  vshz "true" || fail "zombie VM never came back after SIGCONT"
  REFUSED=$(vsudoz "dmesg | grep -c 'not allowed, hostnqn'" || true)
  STALE=$(vsudoz "dmesg | grep -ciE 'stale.*client|EXCHANGE_ID|lease' " || true)
  echo "· zombie resumed: refused-connect lines=$REFUSED, lease/clientid recovery lines=$STALE (both informational)"
  sleep 25
  ZDONE=$(vshz "cat /var/tmp/rig-writer.done 2>/dev/null" || true)
  W3=$(lvol_written); sleep 3; W4=$(lvol_written)
  if [ "${W4:-0}" -gt "${W3:-0}" ]; then
    ZFATE="writing again (fresh incarnation through the re-admit door — grant-site admission re-admitted it)"
  elif [ -n "$ZDONE" ]; then
    ZFATE="errored out ($ZDONE)"
  else
    ZFATE="blocked (no further device bytes)"
  fi
  echo "· zombie fate 30s after resume: $ZFATE"

  # Z8. THE MONEY: the successor's bytes, cold-read at the device,
  # after the zombie has been awake and flailing (or recovering) for
  # half a minute. A single stale-extent write lands here as a mismatch.
  vsudo "echo 3 > /proc/sys/vm/drop_caches"
  SHA_2=$(vsudo "dd if=$MNT/successor.bin bs=1M iflag=direct status=none 2>/var/tmp/succ.dd.err | sha256sum | cut -d' ' -f1; true")
  [ "$SHA_2" = "$SHA_SRC" ] \
    || fail "SUCCESSOR DATA CORRUPTED BY THE RESUMED ZOMBIE ($SHA_2 != $SHA_SRC) — Inv_NoStaleDeviceWrite violated ON REAL HARDWARE"
  echo "✓ successor's 8MiB sha-intact through the zombie's resume — Inv_NoStaleDeviceWrite held at the device"

  # Reap: freeze plumbing down, zombie VM force-stopped (its writer may
  # be mid-D on a dead-or-reborn device; Z0 reboots it clean next run).
  kill "$(cat $PXY_PID_FILE)" 2>/dev/null || true; rm -f "$PXY_PID_FILE"
  limactl stop -f "$ZOMBIE_VM" >/dev/null 2>&1 || true
  vsudo "rm -f /var/tmp/succ.src"

  echo
  echo "✅ zombie-rig PASSED — a client VM FROZEN mid-write (the one zombie shape a"
  echo "   partition cannot make) slept through fence/revoke/auto-unfence, a successor"
  echo "   reused its extents, and on resume the successor's bytes survived: the"
  echo "   eviction barrier + lease-honouring recovery held Inv_NoStaleDeviceWrite"
  echo "   at the device. Zombie's path on wake: $ZFATE. On $KREL / zombie $ZKREL."
  exit 0
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
echo "$UNSTAGE" | grep -c 'record_removed=true' >/dev/null || fail "unstage left the session record: $UNSTAGE"
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

# The other half of §R, and the one that keeps the roller CONVERGING: an
# export nobody is connected to must report zero, or the campaign would
# refuse this node forever (F61's livelock, block edition).
#
# Two rows have to clear, and they clear by different clocks. The node
# ATTACH row goes synchronously with the detach above. The CLIENT-EARNED
# row (minted at LAYOUTGET) is removed by nothing in the normal
# lifecycle, so it stops counting only when the NFS client's lease does —
# promptly if the kernel sent DESTROY_CLIENTID on the last umount,
# otherwise at lease expiry. Hence the poll: what is asserted is that it
# converges, not that it is instant.
BS_N=""
for i in $(seq 1 60); do
  BS=$(vsh "$CSI_CLI block-status --endpoint 127.0.0.1:50051") || fail "block-status after detach"
  BS_N=$(echo "$BS" | head -1 | sed -n 's/.*initiators=\([0-9]*\).*/\1/p')
  [ "${BS_N:-1}" = "0" ] && break
  sleep 1
done
[ "${BS_N:-1}" = "0" ] \
  || fail "block-status still reports $BS_N initiator(s) 60s after unstage+detach — the roller would refuse this node until the volume is deleted: $BS"
# ...and prove WHICH mechanism got it to zero. The client-earned row is
# still in the table — nothing in the normal lifecycle deletes it — so a
# zero here is the LEASE FILTER working, not a row that quietly vanished.
# Without this assertion the check above would pass just as happily on a
# build that had never heard of leases.
HROWS=$(vsh "sqlite3 'file:$RIG/state.db?mode=ro' \
  \"SELECT COUNT(*) FROM block_hosts WHERE volume='$VOL'\"" || echo "?")
[ "${HROWS:-0}" -ge 1 ] \
  || fail "expected the client-earned block_hosts row to OUTLIVE the detach (that is the point of the lease filter); found ${HROWS:-0}"
echo "✓ BlockExportStatus drops to 0 initiators once the session is gone (the roll unblocks, poll $i)"
echo "  …with $HROWS client-earned row(s) still in block_hosts — the LEASE, not a deletion, is what cleared the report"

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
