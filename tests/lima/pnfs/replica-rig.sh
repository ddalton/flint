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

RPC_A="sudo PYTHONPATH=$RIG_TOOLS/py python3 $RIG_TOOLS/scripts/rpc.py -s $SOCK_A"
RPC_B="sudo PYTHONPATH=$RIG_TOOLS/py python3 $RIG_TOOLS/scripts/rpc.py -s $SOCK_B"
GRPC="$RIG_TOOLS/grpcurl -plaintext -import-path $PROTO_DIR -proto pnfs_control.proto"

vsh()  { limactl shell "$LIMA_VM" -- bash -c "$*"; }
vsudo(){ limactl shell "$LIMA_VM" -- sudo bash -c "$*"; }

fail() {
  echo "✗ $*"
  echo "── MDS-A log tail ──"; vsh "tail -40 $RIG_A/mds.log 2>/dev/null" || true
  echo "── MDS-B log tail ──"; vsh "tail -20 $RIG_B/mds.log 2>/dev/null" || true
  exit 1
}

cleanup() {
  set +e
  vsudo "nvme disconnect -n $SUBNQN >/dev/null 2>&1
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
         rm -rf $RIG_A $RIG_B /var/tmp/rig-disk.img /var/tmp/rig-disk-b.img \
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

LEGROW=$(vsh "sudo sqlite3 $RIG_A/state.db \
  \"select target_id||':'||sync_state from block_volume_legs where volume='$VOL' order by target_id\"")
echo "$LEGROW" | tr '\n' ' ' | grep -q "$TGT_B:stale" \
  || fail "A's record does not carry the placed leg as STALE: $LEGROW"
REG=$(vsh "sudo sqlite3 $RIG_A/state.db \
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
STAGE=$(vsudo "env FLINT_NVME_CTRL_LOSS_TMO=30 FLINT_NVME_RECONNECT_DELAY=2 FLINT_NVME_FAST_IO_FAIL=5 \
        $CSI_CLI stage --endpoint 127.0.0.1:50051 --volume-id $VOL --node \$(hostname)") \
  || fail "stage: $STAGE"
NSDEV=$(echo "$STAGE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device"])' 2>/dev/null)
[ -n "$NSDEV" ] || fail "stage reported no device: $STAGE"
vsudo "dd if=/dev/urandom of=$NSDEV bs=1M count=$IO_MIB oflag=direct conv=notrunc status=none" \
  || fail "raw write to the composition"
SHA_SRC=$(vsudo "dd if=$NSDEV bs=1M count=$IO_MIB iflag=direct status=none | sha256sum | cut -d' ' -f1")
echo "✓ V3a: ${IO_MIB} MiB written raw through the composition (sha ${SHA_SRC:0:12}…)"

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
SYNC=$(vsh "sudo sqlite3 $RIG_A/state.db \
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
  SYNC=$(vsh "sudo sqlite3 $RIG_A/state.db \
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

vsudo "$CSI_CLI unstage --volume-id $VOL" >/dev/null 2>&1
echo
echo "✅ replica-rig: every proof held — placement, frame, rebuild, mirror, degrade, rejoin"
