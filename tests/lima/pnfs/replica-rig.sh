#!/usr/bin/env bash
#
# pnfs-block REPLICATION rig: the composition machine against real
# spdk-tgt processes, real NVMe/TCP, and real bytes.
#
# Everything in the block tier's replication workstream — placement, the
# frame, the sparse rebuild, the degrade barrier, expand-under-
# composition — was built model-first and unit-tested at every seam
# against a fake target. NONE of it had moved a byte on hardware. This
# is the rig that decides whether any of it is real.
#
# TOPOLOGY. Two targets, because one target cannot hold two copies and
# a second copy is the entire subject:
#
#   tgt-A  /var/tmp/spdk-rig.sock   lvs_rig    NVMe listener :4420
#   MDS-A  grpc :50051   nfs :20490   FLINT_NODE_NAME=rig-a   ← composer
#   tgt-B  /var/tmp/spdk-rigb.sock  lvs_rigb   NVMe listener :4421
#   MDS-B  grpc :50052   nfs :20491   FLINT_NODE_NAME=rig-b   ← leg host
#
# MDS shards SHARE NOTHING — separate sqlite files, exactly as the chart
# deploys them — so every fact that crosses between them crosses over
# the wire, which is the property that makes this a real two-target
# test rather than two views of one record.
#
# PROOFS, in order of strength:
#   V1. PLACEMENT: HostBlockLeg on MDS-B mints an EMPTY lvol and a leg
#       export admitting exactly rig-a; MDS-A's record carries the leg
#       STALE and the peer's dial coordinates.
#   V2. THE FRAME: MDS-A composes a TWO-SLOT raid1 with ONE member —
#       superblock:false, one slot standing empty for the absent leg —
#       and the client-facing namespace serves the RAID, not the lvol.
#   V3. THE REBUILD: real bytes are written through the staged NVMe
#       session, the reconcile pass rebuilds, and the leg joins the
#       composition: 2 members, record says in-sync.
#   V4. THE MIRROR, BYTE FOR BYTE: the peer's copy is read back over
#       ITS OWN leg export (a second NVMe session, as the composer's
#       host NQN) and sha256 of the written range matches the composer's.
#       This is the proof no unit test can give: the bytes are THERE.
#   V5. THE DEGRADE BARRIER: tgt-B is killed. The unreachability verdict
#       lands, the record goes STALE before the leg leaves the array,
#       and writes CONTINUE — degraded, single-copy, sha-intact.
#   V6. THE REJOIN: tgt-B comes back, the leg is re-hosted, and the
#       rebuild returns the volume to two copies — with the bytes
#       written while it was away.
#
# Exit 0 = every proof held.
#
# Prerequisites: the block-rig's (kernel ≥ 6.11, ~/rig-spdk, cross-built
# release binaries). See tests/lima/pnfs/block-rig.sh's header.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
LIMA_VM="${LIMA_VM:-flint-nfs-client}"
RIG_TOOLS="${RIG_TOOLS:-$HOME/rig-spdk}"
MDS_BIN="$REPO_ROOT/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/flint-pnfs-mds"
CSI_CLI="$REPO_ROOT/spdk-csi-driver/target/aarch64-unknown-linux-musl/release/pnfs-csi-cli"
CFG_A="$REPO_ROOT/tests/lima/pnfs/mds-block.yaml"
CFG_B="$REPO_ROOT/tests/lima/pnfs/mds-block-b.yaml"
PROTO_DIR="$REPO_ROOT/spdk-csi-driver/proto"

VOL=repvol
VOL_BYTES=$((256 * 1024 * 1024))
IO_MIB=32
SOCK_A=/var/tmp/spdk-rig.sock
SOCK_B=/var/tmp/spdk-rigb.sock
RIG_A=/var/tmp/flint-rig
RIG_B=/var/tmp/flint-rig-b
# THE WITNESS: one sqlite file BOTH shards open. Their own records stay
# separate (that is the architecture, and §V1's assertions still read
# them) — what moves here is arbitration alone: the seat, the leg sync
# marks, the serving leases, the target registry and the allow-list.
# A store on one of the two targets cannot arbitrate between them, so
# this file is what makes §V7 possible at all.
WITNESS=/var/tmp/flint-rig-witness.db
TGT_A=rig-a
TGT_B=rig-b
SUBNQN="nqn.2024-11.com.flint:block:$VOL"
LEGNQN="nqn.2024-11.com.flint:leg:$VOL"
RAID="flintraid-$VOL"

# Rig timescales. The verdict needs BOTH a strike count and a wall-clock
# floor (a count alone would be a statement about loop cadence — F60),
# so both are lowered together or the degrade drill waits out 30s of
# production floor for nothing.
RECON_SECS=5
STRIKES=2
UNREACH_SECS=5
PROBE_TMO=2
# The serving lease, and it is the FAILOVER's clock: assembly refuses
# until the deposed composer's lease lapses, because tearing its fan-in
# away while it can still ack is what strands acked writes. 120s in
# production; here it is the horizon §V7 waits out.
LEASE_SECS=10
# The client's ctrl_loss_tmo, and it is deliberately LONG — this is the
# clock §V7c proves the redirect does not wait for. When the composer
# dies the kernel controller does not disappear; it sits in `connecting`
# for this many seconds, retrying an address that will never answer
# again (1800s in production). A redirect observed well inside this
# window cannot be ctrl_loss expiry, which is what makes the assertion
# mean anything.
CTRL_LOSS=600
# FS=1 swaps the raw-device payload for a MOUNTED FILESYSTEM and runs
# §V8 — the question §V7 cannot ask. V7 proves the BYTES survive a
# failover, read back with O_DIRECT by a process that opens the device
# fresh. A mounted ext4 is a different consumer: it holds the device
# open across the redirect, it has dirty pages and in-flight I/O when
# the composer dies, and `errors=remount-ro` means one EIO can take it
# out of service even though every byte is intact. Nothing had ever
# asked what a real consumer does here.
#
# It SKIPS §V4-V6 (the leg-mirror and rejoin legs), which compare raw
# device checksums taken at different times — under a mounted
# filesystem those samples are not stable and the drill would flake
# rather than prove. Those legs run in the default (raw) mode, which is
# where they belong.
FS=${FS:-0}
MNT=/mnt/repfs

RPC_A="sudo PYTHONPATH=$RIG_TOOLS/py python3 $RIG_TOOLS/scripts/rpc.py -s $SOCK_A"
RPC_B="sudo PYTHONPATH=$RIG_TOOLS/py python3 $RIG_TOOLS/scripts/rpc.py -s $SOCK_B"
GRPC="$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto"

vsh()  { limactl shell "$LIMA_VM" -- bash -c "$*"; }
vsudo(){ limactl shell "$LIMA_VM" -- sudo bash -c "$*"; }

fail() {
  echo "✗ $*"
  # The redirect lane's own words, when there are any. Everything about
  # a failover's client half is decided in this log, and a drill that
  # cannot show it can only report the symptom.
  if vsh "test -s /tmp/rig-reestablish.log" 2>/dev/null; then
    echo "── reestablish log (redirect lane) ──"
    vsh "grep -vE 'SPDK_RPC|SPDK_FIX|records=' /tmp/rig-reestablish.log 2>/dev/null | tail -20" || true
  fi
  echo "── MDS-A log tail ──"; vsh "tail -40 $RIG_A/mds.log 2>/dev/null" || true
  echo "── MDS-B log tail ──"; vsh "tail -20 $RIG_B/mds.log 2>/dev/null" || true
  exit 1
}

cleanup() {
  set +e
  vsudo "[ -f /tmp/rig-churn.pid ] && kill -9 -\$(cat /tmp/rig-churn.pid) >/dev/null 2>&1
         rm -f /tmp/rig-churn.pid /tmp/rig-churn.sh /tmp/rig-churn.err /tmp/rig-churn.stop /tmp/rig-churn.count /tmp/rig-reestablish.log
         umount -lf $MNT >/dev/null 2>&1
         nvme disconnect -n $SUBNQN >/dev/null 2>&1
         nvme disconnect -n $LEGNQN >/dev/null 2>&1
         pkill -9 -x flint-pnfs-mds
         # BY EXACT NAME, and both reactors. Two rig-found traps in one
         # line: an spdk_tgt renames its process to reactor_<core>, so
         # the second target (-m 0x2) is 'reactor_1' and killing only
         # 'spdk_tgt'/'reactor_0' leaks it — five survived one session
         # and the next run died with EADDRINUSE on :4421. The obvious
         # fix, pkill -f <path>, is WORSE: the pattern appears in the
         # command line of the very shell running it, so pkill kills
         # ITSELF and every line after it in this cleanup silently never
         # runs — which is how a stale lvstore survived into a fresh run
         # and made create_lvstore fail with 'already claimed'.
         pkill -9 -x spdk_tgt; pkill -9 -x reactor_0; pkill -9 -x reactor_1
         sleep 0.5
         rm -rf $RIG_A $RIG_B $WITNESS ${WITNESS}-wal ${WITNESS}-shm \
                /var/tmp/rig-disk.img /var/tmp/rig-disk-b.img \
                $SOCK_A ${SOCK_A}.lock $SOCK_B ${SOCK_B}.lock /var/tmp/spdk_cpu_lock_*
         rm -f /dev/disk/by-id/nvme-eui.*"
  set -o pipefail
}
# KEEP=1 leaves the whole two-target stack standing on exit — the only
# way to look at a failed proof's actual state, since everything this
# rig asserts lives in processes it would otherwise kill.
trap '[ "${KEEP:-0}" = "1" ] || cleanup' EXIT

# ── 0. preflight ──────────────────────────────────────────────────────
[ -x "$MDS_BIN" ] || { echo "✗ missing $MDS_BIN — cross-build it first"; exit 1; }
[ -x "$CSI_CLI" ] || { echo "✗ missing $CSI_CLI — cross-build it first"; exit 1; }
KREL=$(vsh "uname -r")
KMAJ=${KREL%%.*}; KMIN=$(echo "$KREL" | cut -d. -f2)
if [ "$KMAJ" -lt 6 ] || { [ "$KMAJ" -eq 6 ] && [ "$KMIN" -lt 11 ]; }; then
  echo "✗ VM kernel $KREL < 6.11"; exit 1
fi
vsh "test -x $RIG_TOOLS/spdk_tgt && test -x $RIG_TOOLS/grpcurl" \
  || { echo "✗ $RIG_TOOLS incomplete"; exit 1; }
echo "▶ replica-rig on $LIMA_VM (kernel $KREL)"
cleanup

# ── 1. two targets ────────────────────────────────────────────────────
# Both minimized (--no-huge, small pools) — the block-rig recipe, and
# the sizes are not free parameters: at -s 256 the fsdev subsystem
# cannot allocate its io pool and framework_start_init fails outright.
# Two at -s 512 leave the 2 GB VM ~1 GB, measured.
start_tgt() {
  local sock=$1 img=$2 lvs=$3 rpc=$4 log=$5 cpumask=$6
  vsudo "test -f $img || truncate -s 1G $img"
  vsudo "nohup $RIG_TOOLS/spdk_tgt --no-huge -s 512 -r $sock -m $cpumask --wait-for-rpc >$log 2>&1 &
         sleep 0.5"
  local i
  for i in $(seq 1 20); do
    vsh "$rpc rpc_get_methods >/dev/null 2>&1" && break
    [ "$i" = 20 ] && fail "spdk_tgt on $sock never came up ($log)"
    sleep 0.5
  done
  vsudo "chmod 0777 $sock"
  vsh "$rpc iobuf_set_options --small-pool-count 4096 --large-pool-count 1024" >/dev/null \
    || fail "iobuf_set_options on $sock"
  vsh "$rpc iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1" >/dev/null \
    || fail "iscsi_set_options on $sock"
  vsh "$rpc framework_start_init" >/dev/null || fail "framework_start_init on $sock"
  for i in $(seq 1 60); do
    vsh "$rpc framework_wait_init >/dev/null 2>&1" && break
    [ "$i" = 60 ] && fail "subsystems never initialized on $sock ($log)"
    sleep 0.5
  done
  vsh "$rpc bdev_aio_create $img $(basename "$img" .img) 4096" >/dev/null || fail "bdev_aio_create $img"
  # A RESTART must ADOPT the store, never re-make it: the leg's copy
  # lives in this image and the whole of §V6 is that it survived. SPDK
  # auto-examines the aio bdev and brings an existing lvstore back by
  # itself, so creating one here would fail with 'already claimed' —
  # which is exactly how the first V6 run died.
  if vsh "$rpc bdev_lvol_get_lvstores" | grep -q "\"$lvs\""; then
    echo "  … $lvs adopted from $img (the leg's copy survived the restart)"
  else
    vsh "$rpc bdev_lvol_create_lvstore $(basename "$img" .img) $lvs" >/dev/null \
      || fail "create_lvstore $lvs"
  fi
  vsh "$rpc nvmf_create_transport -t TCP" >/dev/null || fail "nvmf_create_transport on $sock"
}
vsudo "mkdir -p $RIG_A/exports $RIG_B/exports; chmod -R 0777 $RIG_A $RIG_B"
start_tgt "$SOCK_A" /var/tmp/rig-disk.img   lvs_rig  "$RPC_A" "$RIG_A/spdk.log" 0x1
start_tgt "$SOCK_B" /var/tmp/rig-disk-b.img lvs_rigb "$RPC_B" "$RIG_B/spdk.log" 0x2
echo "✓ two spdk_tgt up: A(lvs_rig, :4420) and B(lvs_rigb, :4421)"

# ── 2. two MDS shards ────────────────────────────────────────────────
# FLINT_NODE_NAME is what a seat, a leg row and a registry key all name.
# Distinct values here are exactly the production shape (the downward
# API's spec.nodeName), and they are why these two records can refer to
# each other at all.
start_mds() {
  local cfg=$1 node=$2 log=$3 port=$4
  vsh "env FLINT_PNFS_BLOCK_LAYOUT=1 FLINT_NODE_NAME=$node FLINT_MDS_GRPC_PORT=$port \
        FLINT_PNFS_EXPORT_RECONCILE_SECS=$RECON_SECS \
        FLINT_PNFS_BLOCK_UNREACHABLE_STRIKES=$STRIKES \
        FLINT_PNFS_BLOCK_UNREACHABLE_MIN_SECS=$UNREACH_SECS \
        FLINT_PNFS_BLOCK_PROBE_TIMEOUT_SECS=$PROBE_TMO \
        FLINT_PNFS_WITNESS_SQLITE=$WITNESS \
        FLINT_PNFS_BLOCK_LEASE_SECS=$LEASE_SECS \
        FLINT_NFS_GRACE_SECS=5 RUST_LOG='${MDS_LOG:-info}' \
        nohup $MDS_BIN --config $cfg >>$log 2>&1 & sleep 0.5"
  local i
  for i in $(seq 1 20); do
    vsh "bash -c 'exec 3<>/dev/tcp/127.0.0.1/$port' 2>/dev/null" && break
    [ "$i" = 20 ] && fail "MDS on :$port never came up ($log)"
    sleep 0.5
  done
}
start_mds "$CFG_A" "$TGT_A" "$RIG_A/mds.log" 50051
start_mds "$CFG_B" "$TGT_B" "$RIG_B/mds.log" 50052
echo "✓ two MDS shards up: A(rig-a, :50051) and B(rig-b, :50052) — separate sqlite"

# ── V1. PLACEMENT ────────────────────────────────────────────────────
# The controller's act, played by grpcurl: host the leg on B FIRST, then
# create the volume on A naming it. The order is the product's — a
# create that returned success before its second copy had anywhere to
# live would be a silently single-copy replicated volume, and nothing
# afterwards would go back and make one.
HOSTED=$(vsh "$GRPC -d '{\"volumeId\":\"$VOL\",\"sizeBytes\":$VOL_BYTES,\"composerTarget\":\"$TGT_A\"}' \
          127.0.0.1:50052 pnfs.control.MdsControl/HostBlockLeg") || fail "HostBlockLeg RPC"
echo "$HOSTED" | grep -q '"hosted": true' || fail "HostBlockLeg refused: $HOSTED"

vsh "$RPC_B bdev_get_bdevs -b lvs_rigb/$VOL" >/dev/null 2>&1 \
  || fail "B minted no lvol for the leg"
LEGSUB=$(vsh "$RPC_B nvmf_get_subsystems")
echo "$LEGSUB" | grep -q "$LEGNQN" || fail "B has no leg export $LEGNQN"
LEGHOSTS=$(echo "$LEGSUB" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$LEGNQN':
        print(','.join(h['nqn'] for h in s.get('hosts', []))); break
")
[ "$LEGHOSTS" = "nqn.2024-11.com.flint:node:$TGT_A" ] \
  || fail "leg export admits '$LEGHOSTS', expected exactly the composer"
echo "✓ V1a: B hosts an EMPTY leg, offered to $TGT_A and nobody else"

CV=$(vsh "$GRPC -d '{\"volumeId\":\"$VOL\",\"sizeBytes\":$VOL_BYTES,\"layoutClass\":\"scsi\",
      \"legTarget\":\"$TGT_B\",\"legTraddr\":\"127.0.0.1\",\"legTrsvcid\":4421}' \
      127.0.0.1:50051 pnfs.control.MdsControl/CreateVolume") || fail "CreateVolume RPC"
echo "$CV" | grep -q '"created": true' || fail "CreateVolume refused: $CV"

LEGROW=$(vsh "sudo sqlite3 $WITNESS \
  \"select target_id||':'||sync_state from block_volume_legs where volume='$VOL' order by target_id\"")
echo "$LEGROW" | tr '\n' ' ' | grep -q "$TGT_B:stale" \
  || fail "A's record does not carry the placed leg as STALE: $LEGROW"
REG=$(vsh "sudo sqlite3 $WITNESS \
  \"select traddr||':'||trsvcid from block_targets where target_id='$TGT_B'\"")
[ "$REG" = "127.0.0.1:4421" ] || fail "A cannot dial the leg target: '$REG'"
echo "✓ V1b: A records the leg STALE (an empty copy is not electable) and can dial it"

# ── V2. THE FRAME ────────────────────────────────────────────────────
# A raid's slot count is fixed at creation, so the frame is a function
# of the RECORD's leg count and not of how many legs are healthy. Two
# slots, one member, one waiting.
# The frame is built by the CONVERGE pass, not by CreateVolume — the
# record lands first and the device state follows it, which is the whole
# level-triggered discipline. So this waits for the loop rather than
# racing it.
for i in $(seq 1 20); do
  FRAME=$(vsh "$RPC_A bdev_raid_get_bdevs all")
  echo "$FRAME" | grep -q "$RAID" && break
  [ "$i" = 20 ] && fail "no composition after ${i}s (see $RIG_A/mds.log)"
  sleep 1
done
python3 - "$FRAME" "$RAID" <<'PY' || fail "the frame is not a two-slot raid1"
import json, sys
raids = json.loads(sys.argv[1])
r = next((x for x in raids if x["name"] == sys.argv[2]), None)
assert r, f"no raid named {sys.argv[2]}: {[x['name'] for x in raids]}"
assert r["raid_level"] == "raid1", r["raid_level"]
assert r["superblock"] is False, "a superblock shifts every byte under the pinned NGUID"
# TWO SLOTS: sized by the RECORD's leg count, not by how many legs are
# healthy. Whether the second one is filled yet is a race with the
# rebuild (on an empty volume the copy has nothing to move and finishes
# in milliseconds), so the EMPTY slot is asserted where it is stable:
# §V5 empties it by degrading, §V6 fills it again.
assert r["num_base_bdevs"] == 2, r["num_base_bdevs"]
assert r["state"] == "online", r["state"]
names = [b.get("name") for b in r["base_bdevs_list"]]
assert any(n and n.endswith("/repvol") for n in names), f"local leg absent: {names}"
PY
NSBDEV=$(vsh "$RPC_A nvmf_get_subsystems" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(s['namespaces'][0]['bdev_name']); break
")
[ "$NSBDEV" = "$RAID" ] || fail "the namespace serves '$NSBDEV', not the composition"
echo "✓ V2: two-slot raid1 (superblock:false) sized by the record, and the namespace serves it"

# ── V3. REAL BYTES, THEN THE REBUILD ─────────────────────────────────
vsudo "modprobe nvme-tcp"
STAGE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=$CTRL_LOSS FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
        $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
  || fail "stage: $STAGE"
NSDEV=$(echo "$STAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)
[ -n "$NSDEV" ] || fail "stage reported no device: $STAGE"
if [ "$FS" = "1" ]; then
# THE CONSUMER IS A FILESYSTEM. Everything downstream of here is about
# what a mount does, not what a byte does.
vsudo "mkfs.ext4 -q -F -E lazy_itable_init=0,lazy_journal_init=0 $NSDEV" \
  || fail "mkfs.ext4 on the composition"
vsudo "mkdir -p $MNT && mount -o errors=remount-ro $NSDEV $MNT" \
  || fail "mount the composition"
vsudo "dd if=/dev/urandom of=$MNT/payload.bin bs=1M count=$IO_MIB conv=fsync status=none" \
  || fail "writing the payload through the filesystem"
vsudo "sync"
PAY_MD5=$(vsudo "md5sum $MNT/payload.bin | cut -d' ' -f1")
[ -n "$PAY_MD5" ] || fail "no payload checksum"
echo "✓ V3a: ext4 on the composition, ${IO_MIB} MiB payload written and synced (md5 ${PAY_MD5:0:12}…)"
else
vsudo "dd if=/dev/urandom of=$NSDEV bs=1M count=$IO_MIB oflag=direct conv=notrunc status=none" \
  || fail "raw write to the composition"
SHA_SRC=$(vsudo "dd if=$NSDEV bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
echo "✓ V3a: ${IO_MIB} MiB written raw through the composition (sha ${SHA_SRC:0:12}…)"
fi

# The rebuild is spawned by the reconcile pass — never awaited by it,
# because a full copy can take hours and that loop renews every serving
# lease on the target.
for i in $(seq 1 40); do
  MEMBERS=$(vsh "$RPC_A bdev_raid_get_bdevs all" | python3 -c "
import json,sys
r = next((x for x in json.load(sys.stdin) if x['name'] == '$RAID'), None)
print(len([b for b in r['base_bdevs_list'] if b.get('name')]) if r else 0)
")
  [ "$MEMBERS" = "2" ] && break
  [ "$i" = 40 ] && fail "the leg never joined the composition (see $RIG_A/mds.log)"
  sleep 1
done
SYNC=$(vsh "sudo sqlite3 $WITNESS \
  \"select sync_state from block_volume_legs where volume='$VOL' and target_id='$TGT_B'\"")
[ "$SYNC" = "insync" ] || fail "the leg is a member but the record says '$SYNC'"
vsh "grep -c 'is IN SYNC and back in the composition' $RIG_A/mds.log" >/dev/null \
  || fail "no rebuild completion in the MDS log"
echo "✓ V3b: the rebuild ran — 2 members, record in-sync"

# ── V4. THE MIRROR, BYTE FOR BYTE ────────────────────────────────────
# Read the peer's copy over its OWN leg export, as the composer's host
# NQN (the only identity that export admits). superblock:false is what
# makes this comparison meaningful: with no data offset, leg byte N is
# volume byte N.
# The reader needs an identity of its own. The composer's NQN is taken:
if [ "$FS" = "1" ]; then
echo "  … FS=1: skipping V4-V6 (raw checksum legs; they run in the default mode)"
else
# SPDK already holds a controller under it, and NVMe binds one host NQN
# to one host ID, so the kernel's connect under the same NQN is refused
# (rig-found — the obvious "just read it as the composer" does not work
# and should not).
#
# Which makes the allow-list itself testable on the way past: an
# unadmitted NQN must be REFUSED, and admitting one at the target is
# what lets the read happen at all.
VERIFY_NQN="nqn.2024-11.com.flint:node:rig-verify"
# A host ID of its own, because the kernel binds one host ID to one host
# NQN system-wide: without this the connect fails with "found same
# hostid but different hostnqn" before it ever reaches the target.
VERIFY_HOSTID="11111111-2222-3333-4444-555555555555"
read_leg() {
  local want=$1 label=$2
  local conn="nvme connect -t tcp -a 127.0.0.1 -s 4421 -n $LEGNQN \
              --hostnqn=$VERIFY_NQN --hostid=$VERIFY_HOSTID -i 1"
  vsudo "$conn" >/dev/null 2>&1 \
    && fail "$label: the leg export admitted an NQN its record never named"
  # Tolerated if already present: MDS-B derives this allow-list from the
  # seat every pass, so a leftover from an earlier read may or may not
  # still be there. (That pruning is also a clock on this read — it is
  # sub-second, the pass is every ${RECON_SECS}s.)
  vsh "$RPC_B nvmf_subsystem_add_host $LEGNQN $VERIFY_NQN" >/dev/null 2>&1
  vsudo "$conn" >/dev/null 2>&1 || fail "$label: admitted, and still refused"
  sleep 1
  local dev
  dev=$(vsudo "ls -1 /dev/nvme*n1 | grep -v '^$NSDEV\$' | tail -1")
  [ -n "$dev" ] || fail "$label: the peer's leg namespace did not appear"
  local sha
  sha=$(vsudo "dd if=$dev bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
  vsudo "nvme disconnect -n $LEGNQN >/dev/null 2>&1"
  vsh "$RPC_B nvmf_subsystem_remove_host $LEGNQN $VERIFY_NQN" >/dev/null 2>&1
  [ "$sha" = "$want" ] \
    || fail "$label: THE MIRROR IS NOT A MIRROR — leg ${sha:0:12}… != ${want:0:12}…"
}
read_leg "$SHA_SRC" "V4"
echo "✓ V4: the leg refuses an unnamed host, and its copy is byte-identical over ${IO_MIB} MiB"

# ── V5. THE DEGRADE BARRIER ──────────────────────────────────────────
# Kill the peer's target. The record must go STALE before the leg leaves
# the array — while it is still a member its transport QUEUES, so raid1
# cannot complete a write only one leg took, and no ack can outrun the
# record. Then the leg is removed, the queue drains, and the volume
# serves degraded.
# tgt-B only, by its reactor name (core mask 0x2 ⇒ 'reactor_1'). A
# -f pattern would match this shell's own command line first.
vsudo "pkill -9 -x reactor_1"
echo "  … tgt-B killed; waiting for the verdict (strikes=$STRIKES, floor=${UNREACH_SECS}s)"
for i in $(seq 1 60); do
  SYNC=$(vsh "sudo sqlite3 $WITNESS \
    \"select sync_state from block_volume_legs where volume='$VOL' and target_id='$TGT_B'\"")
  [ "$SYNC" = "stale" ] && break
  [ "$i" = 60 ] && fail "the leg never went stale after its target died"
  sleep 1
done
for i in $(seq 1 30); do
  MEMBERS=$(vsh "$RPC_A bdev_raid_get_bdevs all" | python3 -c "
import json,sys
r = next((x for x in json.load(sys.stdin) if x['name'] == '$RAID'), None)
print(len([b for b in r['base_bdevs_list'] if b.get('name')]) if r else 0)
")
  [ "$MEMBERS" = "1" ] && break
  [ "$i" = 30 ] && fail "the dead leg never left the composition"
  sleep 1
done
# ORDER: the mark is durable BEFORE the array degrades. The log carries
# both lines and the mark's must come first — that is the barrier.
ORDER=$(vsh "grep -n 'marking it STALE before degrading\|now serving DEGRADED' $RIG_A/mds.log | head -2")
echo "$ORDER" | head -1 | grep -q 'marking it STALE before degrading' \
  || fail "the degrade did not mark before removing: $ORDER"
# And the volume still takes writes.
vsudo "dd if=/dev/urandom of=$NSDEV bs=1M count=4 oflag=direct conv=notrunc status=none" \
  || fail "the degraded volume refused a write"
echo "✓ V5: mark-then-degrade in that order, and the degraded volume still writes"

# ── V6. THE REJOIN ───────────────────────────────────────────────────
start_tgt "$SOCK_B" /var/tmp/rig-disk-b.img lvs_rigb "$RPC_B" "$RIG_B/spdk.log" 0x2
start_mds "$CFG_B" "$TGT_B" "$RIG_B/mds.log" 50052
HOSTED=$(vsh "$GRPC -d '{\"volumeId\":\"$VOL\",\"sizeBytes\":$VOL_BYTES,\"composerTarget\":\"$TGT_A\"}' \
          127.0.0.1:50052 pnfs.control.MdsControl/HostBlockLeg") || fail "re-host RPC"
echo "$HOSTED" | grep -q '"hosted": true' || fail "re-host refused: $HOSTED"
for i in $(seq 1 60); do
  MEMBERS=$(vsh "$RPC_A bdev_raid_get_bdevs all" | python3 -c "
import json,sys
r = next((x for x in json.load(sys.stdin) if x['name'] == '$RAID'), None)
print(len([b for b in r['base_bdevs_list'] if b.get('name')]) if r else 0)
")
  [ "$MEMBERS" = "2" ] && break
  [ "$i" = 60 ] && fail "the returned leg never rejoined (see $RIG_A/mds.log)"
  sleep 1
done
# THE SPARSE COPY MOVED REAL BYTES. The FIRST rebuild had nothing to
# carry (an empty volume has no allocated clusters, and copying zero of
# them is the sparseness working); this one must carry the writes the
# leg missed while it was gone.
COPIED=$(vsh "grep 'is IN SYNC and back in the composition' $RIG_A/mds.log | tail -1" \
         | sed -E 's/.* — ([0-9]+) cluster.*/\1/')
[ -n "$COPIED" ] && [ "$COPIED" -gt 0 ] \
  || fail "the rejoin copied $COPIED clusters — the degraded-window writes were not carried"
echo "  … the rebuild carried $COPIED cluster(s) written while the leg was away"
SHA_NOW=$(vsudo "dd if=$NSDEV bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
[ "$SHA_NOW" != "$SHA_SRC" ] || fail "the degraded write did not change the volume — the drill proves nothing"
read_leg "$SHA_NOW" "V6"
echo "✓ V6: the leg rejoined and carries the bytes written while it was gone"
fi  # end of the raw-checksum legs (V4-V6)

# ── V7. THE FAILOVER ─────────────────────────────────────────────────
# Everything above kills the LEG HOST. This kills the COMPOSER, which is
# the case the whole composition machine exists for and the one nothing
# had ever run: the CAS, the eviction horizon, assembly and the client's
# redirect, driven from the side that has to be right.
#
# It is only possible because the two shards now arbitrate in ONE record.
# Before the witness, every fact this section reads — that rig-a is
# unreachable, that rig-b's leg is in sync, that the client was ever
# admitted — lived in the database that dies with the composer.
echo "── V7: the composer dies ──"
if [ "$FS" != "1" ]; then
SHA_PRE=$(vsudo "dd if=$NSDEV bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
fi
CLIENT_NQN="nqn.2024-11.com.flint:node:$(vsh hostname)"
# The client is attached HERE, at A, and its admission was recorded by
# A's shard. Whether B can see it is the allow-list half of the proof.
ADMITTED=$(vsh "sudo sqlite3 $WITNESS \
  \"select count(*) from block_node_attach where volume='$VOL' and host_nqn='$CLIENT_NQN'\"")
[ "$ADMITTED" = "1" ] || fail "the client's admission is not in the witness: '$ADMITTED'"

# Kill tgt-A by its reactor name (core mask 0x1 ⇒ 'reactor_0'). MDS-A
# stays UP on purpose: its record is fine, its disk arm is gone, and a
# composer that cannot serve but can still talk is exactly the world the
# dead-man was written for.
if [ "$FS" = "1" ]; then
# I/O IN FLIGHT WHEN THE COMPOSER DIES, which is the only version of
# this question worth asking. A quiescent mount survives anything: the
# filesystem never issues a request, so it never sees an error, and the
# drill would be reporting that nothing happened. This writer keeps
# real fsync'd traffic on the device across the whole failover and
# counts what it loses.
# IDENTIFIED BY A PIDFILE, not by a pattern. `pkill -f rig-churn` matches
# the command line of the very shell running it (the trap this rig's
# cleanup already documents), and `pkill -x rig-churn.sh` matches
# nothing at all — `-x` compares the process NAME, which for a
# `#!/bin/sh` script is the INTERPRETER (`dash`), never the script. The
# first cost a whole run: the pkill killed its own shell, every cleanup
# line after it silently never ran, and the next run collided with the
# survivors. The second made "the churn writer never started" fire on a
# writer that had.
#
# setsid, because the writer has to outlive the ssh session that starts
# it — a backgrounded child of `limactl shell` goes away with it.
vsudo "rm -f /tmp/rig-churn.err /tmp/rig-churn.pid /tmp/rig-churn.stop
cat > /tmp/rig-churn.sh <<'EOS'
#!/bin/sh
# RUNS UNTIL TOLD TO STOP, not for a fixed count. A counted loop got
# through all 900 iterations in under two seconds — 512 KiB with an
# fsync is a millisecond here — so by the time the drill looked, the
# writer it was about to check had finished and exited cleanly, and
# 'the churn writer never started' was reporting the opposite of what
# happened. The bound below is a runaway guard, not the plan.
echo \$\$ > /tmp/rig-churn.pid
i=0
while [ ! -f /tmp/rig-churn.stop ] && [ \$i -lt 1000000 ]; do
  dd if=/dev/urandom of=$MNT/churn.\$((i%4)) bs=64k count=8 conv=fsync status=none \\
    2>/dev/null || echo x >> /tmp/rig-churn.err
  i=\$((i+1))
done
echo \$i > /tmp/rig-churn.count
EOS
chmod +x /tmp/rig-churn.sh
setsid /tmp/rig-churn.sh >/dev/null 2>&1 < /dev/null &"
sleep 2
CHURN_PID=$(vsudo "cat /tmp/rig-churn.pid 2>/dev/null" | tr -d ' \r')
[ -n "$CHURN_PID" ] && vsudo "kill -0 $CHURN_PID 2>/dev/null" \
  || fail "the churn writer never started (pid='$CHURN_PID')"
echo "  … a churn writer is running against the mount"
fi
vsudo "pkill -9 -x reactor_0"
echo "  … tgt-A killed (MDS-A still running); waiting for B to condemn it"

# THE CAS, observed in the witness rather than in a log line.
for i in $(seq 1 90); do
  SEAT=$(vsh "sudo sqlite3 $WITNESS \
    \"select epoch||':'||composer from block_volume_target where volume='$VOL'\"")
  [ "$SEAT" = "2:$TGT_B" ] && break
  [ "$i" = 90 ] && fail "the seat never moved (still '$SEAT') — see $RIG_B/mds.log"
  sleep 1
done
echo "  … seat moved: $SEAT"
# The deposed leg is marked stale by ASSEMBLY, not by the CAS: between
# them the old composer may still be acking, so its leg is not demoted
# until the new one actually takes the composition.
for i in $(seq 1 60); do
  ASYNC=$(vsh "sudo sqlite3 $WITNESS \
    \"select sync_state from block_volume_legs where volume='$VOL' and target_id='$TGT_A'\"")
  [ "$ASYNC" = "stale" ] && break
  [ "$i" = 60 ] && fail "the deposed leg never went stale — assembly did not complete"
  sleep 1
done
# THE HORIZON WAS REAL: assembly must have REFUSED at least once while
# A's lease still ran. Without that line the failover happened to be
# fast enough to skip the wait, and this drill would not have tested it.
vsh "grep -q 'assembly waits' $RIG_B/mds.log" \
  || fail "assembly never waited out the deposed lease — the horizon was not exercised"
# And A stopped itself: its renewal is refused by a record that no longer
# names it, and its lease lapses, so its own dead-man suspends the export
# it can no longer serve. This is the only exclusion a partitioned
# composer's LOCAL leg has.
# It does NOT happen at once, and waiting is the assertion: the
# dead-man needs BOTH a refused renewal and an EXPIRED lease, so the
# deposed composer keeps serving until the lease it was granted runs
# out. Suspending on the refusal alone would sever a composition that
# was still entitled — this wait IS the horizon, from A's side.
for i in $(seq 1 $((LEASE_SECS + 3 * RECON_SECS + 15))); do
  vsh "grep -q 'SUSPENDED by the dead-man' $RIG_A/mds.log" && break
  [ "$i" = $((LEASE_SECS + 3 * RECON_SECS + 15)) ] \
    && fail "MDS-A never suspended its export after being deposed"
  sleep 1
done
# And the suspension LANDED. The rig's first composer kill caught this
# exact gap: the dead-man decided correctly and then failed to execute,
# because suspension ran through the ordinary converge and that path
# refuses when the lvol probe does not come back — which on a dead tgt
# it never does. A decision that cannot be carried out is not an
# exclusion.
vsh "grep -q 'SUSPENSION FAILED' $RIG_A/mds.log" \
  && fail "the dead-man decided to suspend and could not — the deposed target may still serve"
echo "✓ V7a: CAS → horizon → assembly at $TGT_B, and the deposed composer suspended itself"

# THE DOOR TRAVELLED. B builds its allow-list from the witness, so the
# client admitted at A — a client B's own record has never heard of — is
# on the survivor's export without anyone re-admitting it.
# WAIT for it: assembly and A's self-suspension key off the SAME lease
# lapse, so A can stop a pass before B starts serving — checking once
# here raced the reconcile loop rather than testing it. Everything on
# this tier is level-triggered; assertions about it have to be too.
for i in $(seq 1 $((4 * RECON_SECS + 20))); do
  VOLSUB=$(vsh "$RPC_B nvmf_get_subsystems")
  echo "$VOLSUB" | grep -q "$SUBNQN" && break
  [ "$i" = $((4 * RECON_SECS + 20)) ] \
    && fail "the survivor never built the volume export (see $RIG_B/mds.log)"
  sleep 1
done
VOLHOSTS=$(echo "$VOLSUB" | python3 -c "
import json,sys
for s in json.load(sys.stdin):
    if s.get('nqn') == '$SUBNQN':
        print(','.join(h['nqn'] for h in s.get('hosts', []))); break
")
echo "$VOLHOSTS" | grep -q "$CLIENT_NQN" \
  || fail "the survivor does not admit the client attached at A: '$VOLHOSTS'"
echo "✓ V7b: the survivor's allow-list carries the client admitted at the dead composer"

# THE CLIENT FOLLOWS ON ITS OWN, and it asks the SAME MDS it always
# asked. MDS-A is alive and answers AttachBlockNode by resolving the
# record — which now names B — so the redirect needs no new endpoint and
# no operator: the node is told where the volume lives NOW.
#
# THERE USED TO BE AN `nvme disconnect` HERE, and it was the drill
# standing in for a mechanism that did not exist. tgt-A died, so the
# client's controller is not GONE — it is `connecting`, retrying an
# address that will never answer again, and it stays that way for
# ctrl_loss_tmo (${CTRL_LOSS}s here, 1800s by default). The reconcile
# pass skipped any record whose controller still existed — "present
# (live or reconnecting), not ours to touch" — so the composition
# failed over in seconds and its client followed half an hour later.
# The pass now asks where the volume lives whenever the controller is
# NOT live, and only a MOVED answer interrupts the reconnect; the same
# address, or an MDS it could not reach, still belongs to the reconnect
# policy, or one control-plane outage would become a fleet-wide
# disconnect storm.
#
# THE CLOCK IS THE PROOF: a redirect observed inside this window cannot
# be ctrl_loss expiry, which still has minutes to run.
CTRL_ADDR="for c in /sys/class/nvme/nvme*; do \
  [ \"\$(cat \$c/subsysnqn 2>/dev/null)\" = '$SUBNQN' ] && cat \$c/address 2>/dev/null; done"
ADDR_PRE=$(vsudo "$CTRL_ADDR" | tr -d ' \r')
echo "$ADDR_PRE" | grep -q "trsvcid=4420" \
  || fail "before the redirect the client is not attached to A: '$ADDR_PRE'"
STATE_PRE=$(vsudo "for c in /sys/class/nvme/nvme*; do \
  [ \"\$(cat \$c/subsysnqn 2>/dev/null)\" = '$SUBNQN' ] && cat \$c/state; done" | tr -d ' \r')
[ "$STATE_PRE" != "live" ] \
  || fail "the client's controller still reads 'live' after its target died — the drill \
would prove nothing (fast_io_fail=${FAST_IO_FAIL:-5}s should have taken it out of service)"
echo "  … client controller is '$STATE_PRE' at $ADDR_PRE, ctrl_loss has ${CTRL_LOSS}s to run"
# RETRIED, because that is the contract the tier runs on: the admission
# is durable the moment the record takes it, and the composer's own
# level-triggered pass is what opens its door. A connect landing inside
# that window is refused, and the node agent runs this pass on a timer
# for exactly that reason — so the drill runs it on a timer too (every
# 30s in production; every second here), and a redirect that never
# lands is the real failure.
T0=$(vsh "date +%s")
REDIR_MAX=$((4 * RECON_SECS + 40))
vsudo "rm -f /tmp/rig-reestablish.log"
for i in $(seq 1 $REDIR_MAX); do
  vsudo "$CSI_CLI reestablish >> /tmp/rig-reestablish.log 2>&1"
  ADDR=$(vsudo "$CTRL_ADDR" | tr -d ' \r')
  echo "$ADDR" | grep -q "trsvcid=4421" && break
  [ "$i" = "$REDIR_MAX" ] \
    && fail "the client never followed the volume to B — still at '$ADDR' after ${REDIR_MAX}s"
  sleep 1
done
ELAPSED=$(( $(vsh "date +%s") - T0 ))
# WHICH MECHANISM FIRED, from the node's own log rather than inferred
# from the outcome. connect-before-disconnect keeps the client's
# namespace alive across the move; the fallback restores the path at the
# cost of the namespace, and a mounted consumer pays for that difference.
PATHADD=$(vsudo "grep -c 'carrying the namespace' /tmp/rig-reestablish.log 2>/dev/null" | tr -d ' \r')
FELLBACK=$(vsudo "grep -c 'falling back to disconnect-then-connect' /tmp/rig-reestablish.log 2>/dev/null" | tr -d ' \r')
echo "  … redirect mechanism: path-add=${PATHADD:-0}, fallback=${FELLBACK:-0}"
# THE CONTROLLER INVENTORY, because a redirect is a claim about which
# controllers exist and what they are attached to — and every wrong
# conclusion in this leg so far came from inferring that from a single
# address string instead of listing it.
echo "  … controllers on this subsystem:"
vsudo "for c in /sys/class/nvme/nvme*; do \
  [ \"\$(cat \$c/subsysnqn 2>/dev/null)\" = '$SUBNQN' ] || continue; \
  ns=\$(ls -d \$c/nvme*n* 2>/dev/null | xargs -n1 basename 2>/dev/null | tr '\n' ' '); \
  echo \"       \$(basename \$c): \$(cat \$c/address 2>/dev/null) state=\$(cat \$c/state 2>/dev/null) ns=[\$ns]\"; \
done" 2>/dev/null | tr -d '\r'
if [ "${FELLBACK:-0}" != "0" ]; then
  echo "     $(vsudo "grep -m1 'could not add' /tmp/rig-reestablish.log 2>/dev/null" | tr -d '\r')"
fi
[ "$ELAPSED" -lt $((CTRL_LOSS / 2)) ] \
  || fail "the redirect took ${ELAPSED}s of a ${CTRL_LOSS}s ctrl_loss_tmo — that is the \
timeout expiring, not the pass noticing the volume moved"
# The device the redirect produced, found the way production finds it:
# the udev/eui link keyed by the pinned NGUID. It is the same NGUID
# across the failover, which is what lets a client keep its identity for
# the volume while the target underneath it changes.
NSDEV2=$(vsudo "readlink -f /dev/disk/by-id/nvme-eui.* 2>/dev/null | head -1" | tr -d ' \r')
[ -n "$NSDEV2" ] || fail "the redirect left no eui link — the client has no stable device path"
echo "✓ V7c-pre: the client redirected itself to B in ${ELAPSED}s (ctrl_loss_tmo=${CTRL_LOSS}s) → $NSDEV2"
if [ "$FS" != "1" ]; then
SHA_POST=$(vsudo "dd if=$NSDEV2 bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
[ "$SHA_POST" = "$SHA_PRE" ] \
  || fail "THE BYTES DID NOT SURVIVE THE FAILOVER: ${SHA_POST:0:12}… != ${SHA_PRE:0:12}…"
echo "✓ V7c: the client re-attached to the survivor and read ${IO_MIB} MiB byte-identical"
# And it can still WRITE, which is what makes it a volume rather than a
# snapshot: the survivor serves solo (its peer is the dead A), so this
# also exercises the degrade barrier from the other side.
vsudo "dd if=/dev/urandom of=$NSDEV2 bs=1M count=4 oflag=direct conv=notrunc status=none" \
  || fail "the promoted volume refused a write"
echo "✓ V7d: and the promoted composition takes writes"
fi

# ── V8. THE FILESYSTEM (FS=1) ────────────────────────────────────────
# §V7 proves the BYTES survive: a process opens the device fresh, reads
# with O_DIRECT, and the checksum matches. A mounted filesystem is the
# consumer that was never asked. It held the device open across the
# redirect, it had dirty pages and fsync'd traffic in flight when the
# composer died, and `errors=remount-ro` means one EIO takes it out of
# service with every byte on disk intact.
#
# WHAT IS ASSERTED vs WHAT IS MEASURED, because these are different
# claims: durability is an ASSERTION — the payload's md5 must survive,
# through a remount if it comes to that. Whether the mount rode the
# failover LIVE is a MEASUREMENT, reported either way, because a
# filesystem that goes read-only has still lost nothing and the honest
# answer decides whether a pod must restart or merely wait.
if [ "$FS" = "1" ]; then
# The writer's OWN state first: a process stuck in uninterruptible I/O
# on a dead device reports zero errors forever, so "lost 0 writes" is
# only meaningful next to what the writer was doing when it stopped.
CHURN_STATE=$(vsudo "ps -o stat= -p $CHURN_PID 2>/dev/null" | tr -d ' \r')
# Ask it to stop, then insist. setsid made it a group leader, so the
# negative pid takes its dd with it.
vsudo "touch /tmp/rig-churn.stop; sleep 1
       kill -9 -$CHURN_PID >/dev/null 2>&1; kill -9 $CHURN_PID >/dev/null 2>&1"
sleep 1
CHURN_ERR=$(vsudo "wc -l < /tmp/rig-churn.err 2>/dev/null" | tr -d ' \r')
CHURN_ERR=${CHURN_ERR:-0}
case "$CHURN_STATE" in
  D*) CHURN_NOTE="stuck in uninterruptible I/O (D) — its writes neither landed nor failed" ;;
  "") CHURN_NOTE="already gone" ;;
  *)  CHURN_NOTE="state '$CHURN_STATE'" ;;
esac
# WHERE DID THE MOUNT END UP? The mount table is not the answer: it
# OUTLIVES the device under it, so `findmnt` happily reports `rw` for a
# mount whose every I/O fails. Ask the filesystem to do something.
MNT_OPTS=$(vsudo "findmnt -no OPTIONS $MNT 2>/dev/null" | tr -d '\r')
MNT_LIVE=no
# O_DIRECT, or the probe proves the PAGE CACHE is alive rather than the
# device. A 32 MiB payload fits in this VM's RAM several times over, so
# a buffered read succeeds long after the storage under it has gone —
# which is exactly how this leg once reported a mount as LIVE while the
# next line found it refusing writes.
vsudo "dd if=$MNT/payload.bin of=/dev/null bs=4k count=1 iflag=direct status=none" >/dev/null 2>&1 \
  && MNT_LIVE=yes
# `shutdown` in the options is ext4 saying the filesystem is dead. It
# sits AFTER `rw` in the same string, which is how the first version of
# this leg called a corpse LIVE: it pattern-matched the prefix and the
# kernel's actual verdict was three fields to the right.
# ext4's own verdict lives PAST the `rw` prefix, and this leg has now
# been fooled by two different words in that position: `shutdown` (the
# filesystem is dead) and `emergency_ro` (it took an I/O error and
# flipped itself read-only). The mount flags say `rw` in both cases.
# Read the state, never the prefix.
case "$MNT_OPTS" in
  *shutdown*)     MNT_STATE=shutdown ;;
  *emergency_ro*) MNT_STATE=emergency_ro ;;
  "")             MNT_STATE=gone ;;
  *)              MNT_STATE=rw ;;
esac
case "$MNT_STATE:$MNT_LIVE" in
  rw:yes)          MNT_WORLD="LIVE (read-write, and it reads through O_DIRECT)" ;;
  rw:no)           MNT_WORLD="ZOMBIE (mount table says rw; every I/O fails)" ;;
  emergency_ro:*)  MNT_WORLD="READ-ONLY (errors=remount-ro fired during the outage)" ;;
  shutdown:*)      MNT_WORLD="ZOMBIE (ext4 shut the filesystem down)" ;;
  gone:*)          MNT_WORLD="GONE (the mount did not survive)" ;;
  *)               MNT_WORLD="unrecognised options '$MNT_OPTS' (usable=$MNT_LIVE)" ;;
esac
echo "  … the mount after the failover: $MNT_WORLD"
echo "  … the device the redirect produced: $NSDEV2 (staged on $NSDEV)"
CHURN_ITERS=$(vsudo "cat /tmp/rig-churn.count 2>/dev/null" | tr -d ' \r')
echo "  … churn writer: ${CHURN_ITERS:-?} write(s) attempted, $CHURN_ERR recorded error(s), $CHURN_NOTE"
# DURABILITY IS THE ASSERTION. A remount is allowed to get there — that
# is what a pod restart would do — but the bytes are not negotiable.
# THE DEVICE IS THE ASSERTION connect-and-wait bought. If the head
# survived, the client is on the SAME device node it staged — no
# unmount, no new namespace, and recovery is at worst a remount in
# place. A different device here means the head died and everything
# below is the consolation prize.
[ "$NSDEV2" = "$NSDEV" ] \
  || fail "V8: the redirect produced a DIFFERENT device ($NSDEV2, staged on $NSDEV) — \
the namespace head did not survive, so every consumer holding it is holding a corpse"
REMOUNTED=no
# READS ARE NOT THE TEST. An O_DIRECT read succeeds on a read-only
# filesystem, so `MNT_LIVE=yes` says the device is reachable, not that
# the mount is usable — keying recovery off it left the filesystem
# read-only and failed the next line instead.
if [ "$MNT_STATE" != "rw" ] || [ "$MNT_LIVE" != "yes" ]; then
  # In place first, keeping the device: this is what a node plugin
  # could do for a pod without evicting it. `--options-mode=ignore`
  # because mount(8) otherwise replays the CURRENT option string back
  # at the kernel — including ext4's own `emergency_ro` state flag,
  # which is not a mount parameter and which the kernel then rejects.
  # AND THEN PROVE IT TOOK. ext4 ACCEPTS `remount,rw` after
  # errors=remount-ro has fired — mount(8) returns 0 and the kernel
  # logs "re-mounted" — while leaving the filesystem read-only and
  # `emergency_ro` still in its options. The exit status is not the
  # state; only a write is.
  if vsudo "mount --options-mode=ignore -o remount,rw,errors=remount-ro $NSDEV2 $MNT" \
       >/dev/null 2>&1 \
     && vsudo "touch $MNT/.rw-probe && rm -f $MNT/.rw-probe" >/dev/null 2>&1; then
    REMOUNTED="remount,rw (in place, device kept)"
  else
    vsudo "umount -lf $MNT >/dev/null 2>&1; mount -o errors=remount-ro $NSDEV2 $MNT" \
      || fail "V8: the filesystem could not even be remounted after the failover \
(device $NSDEV2, mount was: $MNT_WORLD)"
    REMOUNTED="unmount+mount (same device, $NSDEV2)"
  fi
fi
PAY_NOW=$(vsudo "md5sum $MNT/payload.bin 2>/dev/null | cut -d' ' -f1")
[ "$PAY_NOW" = "$PAY_MD5" ] \
  || fail "V8: THE FILESYSTEM'S PAYLOAD DID NOT SURVIVE THE FAILOVER: \
${PAY_NOW:-<unreadable>} != $PAY_MD5 (mount was: $MNT_WORLD, remounted=$REMOUNTED)"
echo "✓ V8a: the payload is byte-identical through the failover (remounted=$REMOUNTED)"
# And the filesystem takes writes again — the difference between a
# volume that recovered and one that is merely readable.
WRITE_ERR=$(vsudo "dd if=/dev/urandom of=$MNT/after.bin bs=1M count=4 conv=fsync status=none 2>&1 && sync 2>&1")
if [ $? -ne 0 ]; then
  echo "── the write's own words ──"
  echo "   dd: ${WRITE_ERR:-<silent>}"
  echo "   $(vsudo "df -h $MNT | tail -1" 2>/dev/null | tr -d '\r')"
  echo "   $(vsudo "findmnt -no OPTIONS $MNT" 2>/dev/null | tr -d '\r')"
  vsudo "dmesg | tail -6" 2>/dev/null | sed 's/^/   /'
  fail "V8: the filesystem refused a write after the failover (mount: $MNT_WORLD, \
remounted=$REMOUNTED)"
fi
echo "✓ V8b: and it takes writes again"
echo "✓ V8c: THE DEVICE SURVIVED — $NSDEV throughout, no new namespace, nothing to re-stage"
case "$REMOUNTED" in
  no)
    echo "✓ V8d: and the mount RODE IT LIVE — read-write the whole way, $CHURN_ERR lost write(s)"
    ;;
  "unmount+mount"*)
    echo "⚠ V8d: the mount went READ-ONLY and needed an unmount+mount — ON THE SAME DEVICE"
    echo "   ext4 saw $CHURN_ERR real I/O errors while the volume had no path, and"
    echo "   errors=remount-ro did what it promises. Note what does NOT work: ext4"
    echo "   ACCEPTS 'mount -o remount,rw' afterwards — mount(8) returns 0, the kernel"
    echo "   logs 're-mounted' — and stays read-only, so in-place recovery is not"
    echo "   available and a pod must restart. What connect-and-wait bought is that the"
    echo "   DEVICE NODE never changed: no new namespace, no stale device, nothing to"
    echo "   re-stage at the storage layer."
    echo "   To ride it read-WRITE the I/O has to QUEUE instead of erroring, i.e."
    echo "   fast_io_fail_tmo (${FAST_IO_FAIL:-5}s here) must exceed the whole failover"
    echo "   window — which the serving lease dominates (${LEASE_SECS}s here, 120s in"
    echo "   production). That is a real trade against the D-state wedge class."
    ;;
  "remount,rw"*)
    echo "⚠ V8d: the mount went READ-ONLY and came back with a remount in place"
    echo "   ext4 saw $CHURN_ERR real I/O errors while the volume had no path, and"
    echo "   errors=remount-ro did exactly what it promises. Nothing was lost and the"
    echo "   DEVICE never changed, so recovery is 'mount -o remount,rw' — no unmount,"
    echo "   no new device node, no pod eviction. That is the difference connect-and-"
    echo "   wait-before-disconnect bought; before it, this needed a fresh namespace."
    echo "   To ride it read-WRITE the I/O has to QUEUE instead of erroring, i.e."
    echo "   fast_io_fail_tmo (${FAST_IO_FAIL:-5}s here) must exceed the whole failover"
    echo "   window — which the serving lease dominates (${LEASE_SECS}s here, 120s in"
    echo "   production). That is a real trade against the D-state wedge class, not an"
    echo "   oversight."
    ;;
  *)
    echo "⚠ V8d: the mount needed a $REMOUNTED — $MNT_WORLD"
    ;;
esac
fi

# The mount holds the device open: unstage would refuse (or leak a
# dangling mount into the next run) with it standing.
[ "$FS" = "1" ] && vsudo "umount -lf $MNT >/dev/null 2>&1"
vsudo "$CSI_CLI unstage --volume-id $VOL" >/dev/null 2>&1
echo
if [ "$FS" = "1" ]; then
echo "✅ replica-rig FS=1: every proof held — placement, frame, rebuild, FAILOVER, and a MOUNTED FILESYSTEM across it"
else
echo "✅ replica-rig: every proof held — placement, frame, rebuild, mirror, degrade, rejoin, FAILOVER"
fi
