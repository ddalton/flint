#!/usr/bin/env bash
#
# restage-drill.sh — F29: a stale "staged" verdict in kubelet's cache
# must not wedge the volume forever.
#
# Runs INSIDE the Lima VM as root (the repo is visible at the same
# path as on the host):
#
#   limactl shell flint-nfs-client -- sudo \
#     DRIVER_BIN=<path> EXPECT=heal bash tests/lima/f29/restage-drill.sh
#
# The rig is the REAL csi-driver node service (CSI_MODE=node) over a
# real ext4 on a real block device; only spdk-tgt and the Kubernetes
# API are mocked (mock-rig.py — see its header for why that does not
# weaken the verdict). The drill drives the exact call sequence
# kubelet drives, through the CSI gRPC socket, with grpcurl.
#
# The wedge, reproduced literally: stage + publish + write, then
# unpublish and umount the staging path OUT-OF-BAND — kubelet's
# actual-state-of-world still says "staged", so from here the only
# call the driver will ever see again for this volume is publish.
#
#   EXPECT=wedge  (pre-fix binary): publish must FAIL with the F29
#     refusal, twice — proving both that the wedge is permanent and
#     that this drill's oracle can SEE the failure (the anti-vacuity
#     control for the heal run).
#   EXPECT=heal   (fixed binary): publish must self-heal — restage,
#     re-probe, bind-mount — and the payload written before the wedge
#     must read back byte-identical. Then the harder legs: device
#     also torn down; stage made impossible (fault-injected) and the
#     refusal must SURVIVE; fault cleared and the next publish must
#     recover; a DEAD (read-only) staging mount must heal too.
#
# PASS/FAIL is tallied per leg; any FAIL exits non-zero.

set -uo pipefail

DRIVER_BIN="${DRIVER_BIN:?set DRIVER_BIN to the csi-driver binary}"
EXPECT="${EXPECT:?set EXPECT=wedge or EXPECT=heal}"
RIG=/var/tmp/f29rig
REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
PROTO_DIR="$REPO/spdk-csi-driver/proto"
MOCK="$REPO/tests/lima/f29/mock-rig.py"
CSI_SOCK="$RIG/csi.sock"
SPDK_SOCK="$RIG/spdk.sock"
STAGE="$RIG/stage"
TARGET="$RIG/target"
VOL=vol-f29
BDEV=f29lvol
K8S_PORT=18811
AGENT_PORT=19081

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL: $*"; }
leg()  { echo; echo "=== LEG $* ==="; }

# ── teardown of any previous run (pidfile-scoped: never kill by name) ──
cleanup() {
  for pf in "$RIG"/mock.pid "$RIG"/driver.pid; do
    if [ -f "$pf" ]; then
      kill "$(cat "$pf")" 2>/dev/null
      rm -f "$pf"
    fi
  done
  umount -l "$TARGET" 2>/dev/null
  umount -l "$STAGE" 2>/dev/null
  for img in "$RIG"/bdev-*.img; do
    [ -e "$img" ] || continue
    loopdev=$(losetup -j "$img" | cut -d: -f1)
    [ -n "$loopdev" ] && losetup -d "$loopdev" 2>/dev/null
  done
  for l in /dev/ublkb*; do
    [ -L "$l" ] && rm -f "$l"   # symlinks only — never a real device
  done
}
cleanup
rm -rf "$RIG"
mkdir -p "$RIG" "$STAGE" "$TARGET"
trap cleanup EXIT

# ── grpcurl (pinned; installed once) ──────────────────────────────────
if ! command -v grpcurl >/dev/null 2>&1; then
  echo "installing grpcurl 1.9.3 (arm64)..."
  curl -fsSL -o /tmp/grpcurl.tgz \
    https://github.com/fullstorydev/grpcurl/releases/download/v1.9.3/grpcurl_1.9.3_linux_arm64.tar.gz
  tar -C /usr/local/bin -xzf /tmp/grpcurl.tgz grpcurl
fi

# ── mock rig up ───────────────────────────────────────────────────────
F29_RIG="$RIG" F29_SPDK_SOCK="$SPDK_SOCK" F29_HTTP_PORT=$K8S_PORT \
  F29_NODE_NAME=f29-node F29_FAULTS="$RIG/faults.json" \
  nohup python3 "$MOCK" > "$RIG/mock.log" 2>&1 &
echo $! > "$RIG/mock.pid"
for _ in $(seq 1 50); do [ -S "$SPDK_SOCK" ] && break; sleep 0.2; done
[ -S "$SPDK_SOCK" ] || { echo "mock rig never bound $SPDK_SOCK"; cat "$RIG/mock.log"; exit 2; }
# The socket file existing is not the mock living: its second bind can
# fail after the file is already on disk.
sleep 0.5
kill -0 "$(cat "$RIG/mock.pid")" 2>/dev/null \
  || { echo "mock rig died after binding"; cat "$RIG/mock.log"; exit 2; }

cat > "$RIG/kubeconfig" <<EOF
apiVersion: v1
kind: Config
clusters:
- name: f29
  cluster: { server: "http://127.0.0.1:$K8S_PORT" }
contexts:
- name: f29
  context: { cluster: f29, user: f29 }
current-context: f29
users:
- name: f29
  user: {}
EOF

# ── driver up (node mode — the production role for this path) ─────────
env CSI_MODE=node NODE_ID=f29-node HOSTNAME=f29-node \
  KUBECONFIG="$RIG/kubeconfig" FLINT_NAMESPACE=default \
  SPDK_RPC_URL="unix://$SPDK_SOCK" CSI_ENDPOINT="unix://$CSI_SOCK" \
  NODE_AGENT_PORT=$AGENT_PORT BLOCK_DEVICE_BACKEND=ublk \
  UBLK_NUM_QUEUES=1 RUST_LOG=info \
  nohup "$DRIVER_BIN" > "$RIG/driver.log" 2>&1 &
echo $! > "$RIG/driver.pid"
for _ in $(seq 1 100); do [ -S "$CSI_SOCK" ] && break; sleep 0.2; done
[ -S "$CSI_SOCK" ] || { echo "driver never bound $CSI_SOCK"; tail -30 "$RIG/driver.log"; exit 2; }

grpc() { # $1 = Node method, $2 = request json; combined output on stdout
  # unix:// scheme, not -unix: grpcurl 1.9.3's -unix flag still dials
  # tcp (observed on this rig); the scheme form dials the socket.
  grpcurl -plaintext -import-path "$PROTO_DIR" -proto csi.proto \
    -d "$2" "unix://$CSI_SOCK" "csi.v1.Node/$1" 2>&1
}

STAGE_REQ=$(cat <<EOF
{"volume_id":"$VOL","staging_target_path":"$STAGE",
 "publish_context":{"volumeType":"local","bdevName":"$BDEV"},
 "volume_capability":{"mount":{"fs_type":"ext4"},
                      "access_mode":{"mode":"SINGLE_NODE_WRITER"}}}
EOF
)
PUBLISH_REQ=$(cat <<EOF
{"volume_id":"$VOL","staging_target_path":"$STAGE","target_path":"$TARGET",
 "publish_context":{"volumeType":"local","bdevName":"$BDEV"},
 "volume_capability":{"mount":{"fs_type":"ext4"},
                      "access_mode":{"mode":"SINGLE_NODE_WRITER"}}}
EOF
)
UNPUBLISH_REQ="{\"volume_id\":\"$VOL\",\"target_path\":\"$TARGET\"}"

mounted() { mountpoint -q "$1"; }

# ── LEG 1: stage — the fresh path (wipefs + mkfs + mount) ─────────────
leg "1: NodeStage (fresh volume)"
out=$(grpc NodeStageVolume "$STAGE_REQ"); rc=$?
if [ $rc -eq 0 ] && mounted "$STAGE"; then ok "staged; $STAGE is a mountpoint"
else bad "stage rc=$rc; output: $out"; fi

# ── LEG 2: publish + write the payload the wedge must not lose ────────
leg "2: NodePublish + payload write"
out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
if [ $rc -eq 0 ] && mounted "$TARGET"; then ok "published; $TARGET is a mountpoint"
else bad "publish rc=$rc; output: $out"; fi
dd if=/dev/urandom of="$TARGET/payload" bs=1M count=4 status=none
SUM=$(sha256sum "$TARGET/payload" | cut -d' ' -f1)
sync
[ -n "$SUM" ] && ok "payload written (sha256 $SUM)"

# ── LEG 3: unpublish (pod gone), then the WEDGE ───────────────────────
leg "3: unpublish + out-of-band staging umount (the F29 state)"
out=$(grpc NodeUnpublishVolume "$UNPUBLISH_REQ"); rc=$?
[ $rc -eq 0 ] && ok "unpublished" || bad "unpublish rc=$rc; output: $out"
umount "$STAGE" || bad "could not umount staging out-of-band"
if ! mounted "$STAGE"; then ok "staging unmounted; kubelet's cache would still say staged"
else bad "staging still mounted"; fi

# ── LEG 4: the republish kubelet retries forever ──────────────────────
if [ "$EXPECT" = wedge ]; then
  leg "4: republish against the PRE-FIX binary — the wedge must be visible"
  for attempt in 1 2; do
    out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
    if [ $rc -ne 0 ] && echo "$out" | grep -q "restage required (F29)"; then
      ok "attempt $attempt refused with the F29 wedge error"
    else
      bad "attempt $attempt: rc=$rc; output: $out"
    fi
    mounted "$TARGET" && bad "target mounted despite refusal" \
                      || ok "attempt $attempt left target unmounted"
  done
else
  leg "4: republish — the self-heal (staging gone, device still present)"
  out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
  if [ $rc -eq 0 ] && mounted "$TARGET" && mounted "$STAGE"; then
    ok "publish healed: staging restaged + target bound"
  else
    bad "publish rc=$rc; output: $out"
  fi
  got=$(sha256sum "$TARGET/payload" 2>/dev/null | cut -d' ' -f1)
  [ "$got" = "$SUM" ] && ok "payload intact after heal" \
                      || bad "payload sha256 $got != $SUM"

  # ── LEG 5: wedge again, this time the DEVICE is gone too ────────────
  leg "5: heal with device torn down (loop detached, node rebooted shape)"
  grpc NodeUnpublishVolume "$UNPUBLISH_REQ" >/dev/null
  umount "$STAGE"
  for l in /dev/ublkb*; do
    [ -L "$l" ] || continue
    losetup -d "$(readlink -f "$l")" 2>/dev/null
    rm -f "$l"
  done
  [ ! -e /dev/ublkb0 ] || bad "device teardown failed"
  out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
  # The re-materialized device may carry a NEW ublk id: the agent's
  # allocator remembers ids it handed out, and this teardown was
  # out-of-band. Any ublk symlink serving the mount is correct.
  devs=$(find /dev -maxdepth 1 -name 'ublkb*' -type l | wc -l)
  if [ $rc -eq 0 ] && mounted "$TARGET" && [ "$devs" -ge 1 ]; then
    ok "publish healed: device re-materialized ($devs) + restaged"
  else
    bad "publish rc=$rc devs=$devs; output: $out"
  fi
  got=$(sha256sum "$TARGET/payload" 2>/dev/null | cut -d' ' -f1)
  [ "$got" = "$SUM" ] && ok "payload intact across device teardown (blkid guard held)" \
                      || bad "payload sha256 $got != $SUM"
  # ≥2: leg 4's heal already logged one — this leg must have logged its own.
  guards=$(grep -c "No marker but the device carries a filesystem signature" "$RIG/driver.log")
  [ "$guards" -ge 2 ] \
    && ok "restage took the signature-guard arm, not the wipefs arm ($guards)" \
    || bad "signature-guard log count $guards < 2 — how did the data survive?"

  # ── LEG 6: stage made impossible — the refusal must survive the heal ─
  leg "6: fault-injected restage — refusal preserved, then recovery"
  grpc NodeUnpublishVolume "$UNPUBLISH_REQ" >/dev/null
  umount "$STAGE"
  for l in /dev/ublkb*; do
    [ -L "$l" ] || continue
    losetup -d "$(readlink -f "$l")" 2>/dev/null
    rm -f "$l"
  done
  echo '{"ublk_start_disk":"error"}' > "$RIG/faults.json"
  out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
  if [ $rc -ne 0 ] && echo "$out" | grep -q "F29 self-heal restage failed"; then
    ok "unstageable volume still refused (no blind bind)"
  else
    bad "expected restage failure; rc=$rc; output: $out"
  fi
  mounted "$TARGET" && bad "target mounted despite failed restage" \
                    || ok "target left unmounted"
  rm -f "$RIG/faults.json"
  out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
  got=$(sha256sum "$TARGET/payload" 2>/dev/null | cut -d' ' -f1)
  if [ $rc -eq 0 ] && [ "$got" = "$SUM" ]; then
    ok "fault cleared -> next kubelet retry heals, payload intact"
  else
    bad "recovery publish rc=$rc sha=$got; output: $out"
  fi

  # ── LEG 7: the Dead arm — staging mounted but I/O refused ───────────
  leg "7: DEAD staging mount (read-only underneath) heals in one publish"
  grpc NodeUnpublishVolume "$UNPUBLISH_REQ" >/dev/null
  mount -o remount,ro "$STAGE" || bad "could not remount staging ro"
  out=$(grpc NodePublishVolume "$PUBLISH_REQ"); rc=$?
  got=$(sha256sum "$TARGET/payload" 2>/dev/null | cut -d' ' -f1)
  rw=$(findmnt -n -o OPTIONS "$STAGE" | grep -c '^rw')
  if [ $rc -eq 0 ] && [ "$got" = "$SUM" ] && [ "$rw" = 1 ]; then
    ok "dead mount unmounted + restaged rw in one publish, payload intact"
  else
    bad "publish rc=$rc sha=$got rw=$rw; output: $out"
  fi
fi

# ── epilogue: the driver must have survived all of it ─────────────────
leg "8: driver health"
if kill -0 "$(cat "$RIG/driver.pid")" 2>/dev/null; then ok "driver alive"
else bad "driver died"; fi
panics=$(grep -c "panicked" "$RIG/driver.log")
[ "$panics" = 0 ] && ok "no panics in driver log" || bad "$panics panic(s) in driver log"

echo
echo "F29-DRILL($EXPECT) RESULT: PASS=$PASS FAIL=$FAIL"
[ $FAIL -eq 0 ]
