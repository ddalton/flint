#!/usr/bin/env bash
#
# Flint-lite drill — the standalone hub serves two DISTINCT NFS clients.
#
# The flint-lite plan's L0: ONE flint-pnfs-mds in `mode: standalone`
# (no DS fleet, layouts off, every byte MDS-lane) must give two
# independent kernel NFS clients — stand-ins for pods in two K8s
# compute clusters — the full agent-fleet semantics:
#
#   leg 1  distinctness  — different superblocks or every later leg is
#          vacuous (see cross-cluster-drill.sh, whose harness this is).
#   leg 2  close-to-open — A writes+closes, B reads byte-identical;
#          then the reverse.
#   leg 3  cross-client lock exclusion — the server is the only arbiter.
#   leg 4  agent-tools battery — sqlite and git from both clients.
#   leg 5  standalone oracle — the posture is REAL: the server logged
#          the standalone banner, no LAYOUTGET was ever even requested
#          (EXCHANGE_ID advertised non-pNFS, so the client never asks),
#          the export tree grew, a cache-cold read holds connections to
#          the MDS port only, and the F67/0444 permission-denied shapes
#          never appeared (moot without layouts — this asserts it).
#   leg 6  measurement (not an assertion) — metadata rates and MDS-lane
#          sequential throughput: the lite baseline numbers.
#
# Servers run on the macOS host; the Lima VM is the client. Client B is
# a separate netns + UTS hostname (own nfs_net, own co_ownerid) — the
# only honest second client one VM allows. KEEP=1 leaves the rig
# standing. Cleanup umounts BEFORE killing the server (a dead server
# under a live mount D-states umount).
#
# Exit status: 0 on PASS, 1 on FAIL. Suitable as a Makefile target.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN_DIR="$REPO_ROOT/spdk-csi-driver/target/release"
CFG_DIR="$REPO_ROOT/tests/lima/pnfs"
LOG_DIR=/tmp
PIDFILE_DIR=/tmp

LIMA_VM="${LIMA_VM:-flint-nfs-client}"
MDS_PORT=20490
EXPORT_DIR=/tmp/flint-lite-export
MDS_LOG="$LOG_DIR/flint-lite-mds.log"

MNT_A=/mnt/lite-a
MNT_B=/mnt/lite-b
NETNS=liteb
VETH_HOST=liteb0
VETH_NS=liteb1
NS_NET=10.99.78.0/30
NS_GW=10.99.78.1
NS_IP=10.99.78.2
DRILL_MIB=64

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
skip() { echo "  △ SKIP: $*"; }
fail() { echo "  ✗ FAIL: $*"; FAILED=1; exit 1; }

vm()   { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }
# Client B = inside the netns. nsenter --net only (never ip netns exec:
# that clones a private mount namespace and the NFS mount evaporates
# with it). The kernel pins a mount's transports to the netns it was
# created in, so B's traffic stays in $NETNS wherever we run I/O from.
vmb()  { limactl shell "$LIMA_VM" -- sudo ip netns exec "$NETNS" sh -c "$1"; }

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving server, mounts and netns standing"
    return
  fi
  # Umount FIRST: killing the server under a live mount leaves umount
  # D-stated in rpc_wait and only a VM force-restart clears it.
  vm "umount -lf $MNT_B 2>/dev/null; umount -lf $MNT_A 2>/dev/null; \
      ip netns del $NETNS 2>/dev/null; ip link del $VETH_HOST 2>/dev/null; \
      iptables -t nat -D POSTROUTING -s $NS_NET -j MASQUERADE 2>/dev/null" \
      2>/dev/null
  [ -f "$PIDFILE_DIR/flint-lite-mds.pid" ] && \
    kill "$(cat "$PIDFILE_DIR/flint-lite-mds.pid")" 2>/dev/null
  rm -f "$PIDFILE_DIR/flint-lite-mds.pid"
  pkill -9 -f "flint-pnfs-mds --config $CFG_DIR/lite.yaml" 2>/dev/null
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " flint-lite drill — one standalone hub, two distinct NFS clients"
echo "══════════════════════════════════════════════════════════════════"

# ── pre-flight ─────────────────────────────────────────────────────────
[ -x "$BIN_DIR/flint-pnfs-mds" ] \
  || fail "missing $BIN_DIR/flint-pnfs-mds — run 'make build-pnfs'"
command -v limactl >/dev/null || fail "limactl not found (brew install lima)"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running — run 'make lima-up'"
for tool in ip iptables findmnt ss md5sum unshare nsenter; do
  vm "command -v $tool >/dev/null" \
    || fail "VM lacks '$tool' — the netns client needs it"
done

# Fresh world.
cleanup
rm -rf "$EXPORT_DIR"
mkdir -p "$EXPORT_DIR"
chmod 0777 "$EXPORT_DIR"

# ── start the standalone hub — ONE process, that is the point ─────────
say "starting the standalone hub (no DS fleet)"
nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/lite.yaml" \
  >"$MDS_LOG" 2>&1 &
echo $! >"$PIDFILE_DIR/flint-lite-mds.pid"
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-lite-mds.pid")" 2>/dev/null \
  || { tail -20 "$MDS_LOG"; fail "hub died on startup"; }
grep -qc "STANDALONE" "$MDS_LOG" >/dev/null \
  || fail "hub log never announced the standalone posture — is lite.yaml mode: standalone?"
pass "hub up on :$MDS_PORT, standalone posture announced"

HOST_IP=$(vm "getent hosts host.lima.internal | awk '{print \$1}'" | tr -d '\r')
[ -n "$HOST_IP" ] || fail "could not resolve host.lima.internal in the VM"

# ── client A: plain mount in the VM's root namespace ──────────────────
say "mounting client A (root netns)"
vm "mountpoint -q $MNT_A && umount -lf $MNT_A; mkdir -p $MNT_A; \
    timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
      $HOST_IP:/ $MNT_A" || fail "client A mount failed"
pass "client A mounted at $MNT_A"

# ── client B: separate netns + UTS hostname = a different client ──────
say "building netns '$NETNS' and mounting client B from inside it"
vm "ip netns del $NETNS 2>/dev/null; ip link del $VETH_HOST 2>/dev/null; true"
vm "ip netns add $NETNS && \
    ip link add $VETH_HOST type veth peer name $VETH_NS && \
    ip link set $VETH_NS netns $NETNS && \
    ip addr add $NS_GW/30 dev $VETH_HOST && ip link set $VETH_HOST up && \
    ip netns exec $NETNS ip addr add $NS_IP/30 dev $VETH_NS && \
    ip netns exec $NETNS ip link set $VETH_NS up && \
    ip netns exec $NETNS ip link set lo up && \
    ip netns exec $NETNS ip route add default via $NS_GW && \
    sysctl -qw net.ipv4.ip_forward=1 && \
    { iptables -C FORWARD -s $NS_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -s $NS_NET -j ACCEPT; } && \
    { iptables -C FORWARD -d $NS_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -d $NS_NET -j ACCEPT; } && \
    { iptables -t nat -C POSTROUTING -s $NS_NET -j MASQUERADE 2>/dev/null || \
      iptables -t nat -A POSTROUTING -s $NS_NET -j MASQUERADE; }" \
  || fail "netns plumbing failed"
vm "mkdir -p $MNT_B"
vm "nsenter --net=/var/run/netns/$NETNS unshare --uts sh -c ' \
      hostname lite-cluster-b && \
      timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
        $HOST_IP:/ $MNT_B'" || fail "client B mount failed (netns route to host?)"
pass "client B mounted at $MNT_B from netns $NETNS as lite-cluster-b"

# ── leg 1: distinctness — different superblocks or the drill is void ──
say "leg 1: the two mounts must be different superblocks"
SB_A=$(vm "findmnt -no MAJ:MIN $MNT_A" | tr -d ' \r')
SB_B=$(vm "findmnt -no MAJ:MIN $MNT_B" | tr -d ' \r')
[ -n "$SB_A" ] && [ -n "$SB_B" ] || fail "could not read superblock ids"
[ "$SB_A" != "$SB_B" ] \
  || fail "BOTH MOUNTS SHARE SUPERBLOCK $SB_A — one nfs_client, one page \
cache; every coherence leg below would pass vacuously."
pass "distinct superblocks ($SB_A vs $SB_B) — two real clients"

# ── leg 2: close-to-open, both directions ─────────────────────────────
say "leg 2: close-to-open coherence"
ZERO_MD5=$(python3 -c "
import hashlib
h = hashlib.md5()
z = bytes(1024 * 1024)
for _ in range($DRILL_MIB):
    h.update(z)
print(h.hexdigest())")
vm "dd if=/dev/urandom of=$MNT_A/shared.bin bs=1M count=$DRILL_MIB status=none conv=fsync" \
  || fail "client A write failed"
MD5_A=$(vm "md5sum $MNT_A/shared.bin" | awk '{print $1}' | tr -d '\r')
MD5_B=$(vmb "md5sum $MNT_B/shared.bin" | awk '{print $1}' | tr -d '\r')
[ -n "$MD5_B" ] || fail "client B could not read A's file"
[ "$MD5_B" = "$MD5_A" ] || fail "A wrote $MD5_A, B read $MD5_B — close-to-open broken"
[ "$MD5_B" != "$ZERO_MD5" ] || fail "B read ALL ZEROS — the F67 shape, cross-client"
pass "A→B: ${DRILL_MIB} MiB byte-identical ($MD5_A)"
vmb "dd if=/dev/urandom of=$MNT_B/reply.bin bs=1M count=8 status=none conv=fsync" \
  || fail "client B write failed"
MD5_B2=$(vmb "md5sum $MNT_B/reply.bin" | awk '{print $1}' | tr -d '\r')
MD5_A2=$(vm "md5sum $MNT_A/reply.bin" | awk '{print $1}' | tr -d '\r')
[ "$MD5_A2" = "$MD5_B2" ] || fail "B wrote $MD5_B2, A read $MD5_A2"
pass "B→A: 8 MiB byte-identical ($MD5_B2)"

# ── leg 3: cross-client POSIX lock exclusion ──────────────────────────
say "leg 3: byte-range lock held via A must refuse B (server-arbitrated)"
if ! vm "command -v python3 >/dev/null"; then
  skip "no python3 in VM — lock leg needs fcntl"
else
  vm "rm -f /tmp/lite-lock-a.log; \
      cat >/tmp/lite-lock-a.py <<'PY'
import fcntl, time
f = open('$MNT_A/shared.bin', 'r+b')
fcntl.lockf(f, fcntl.LOCK_EX, 4096, 0)
print('A_LOCKED', flush=True)
time.sleep(8)
fcntl.lockf(f, fcntl.LOCK_UN, 4096, 0)
print('A_UNLOCKED', flush=True)
PY
      setsid python3 /tmp/lite-lock-a.py >/tmp/lite-lock-a.log 2>&1 </dev/null & \
      for i in \$(seq 50); do grep -q A_LOCKED /tmp/lite-lock-a.log 2>/dev/null && break; sleep 0.2; done; \
      grep -c A_LOCKED /tmp/lite-lock-a.log" | grep -q '^1' \
    || fail "client A never acquired its lock"
  B_RESULT=$(vmb "python3 - <<'PY'
import fcntl, time, errno
f = open('$MNT_B/shared.bin', 'r+b')
try:
    fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, 4096, 0)
    print('B_GOT_LOCK_WHILE_A_HELD')
except OSError as e:
    if e.errno in (errno.EAGAIN, errno.EACCES):
        print('B_REFUSED')
    else:
        print('B_ERRNO_%d' % e.errno)
    deadline = time.time() + 20
    while time.time() < deadline:
        try:
            fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, 4096, 0)
            print('B_ACQUIRED_AFTER_RELEASE')
            break
        except OSError:
            time.sleep(0.5)
    else:
        print('B_NEVER_ACQUIRED')
PY" | tr -d '\r')
  case "$B_RESULT" in
    *B_GOT_LOCK_WHILE_A_HELD*)
      fail "B took the lock WHILE A held it — no server arbitration" ;;
    *B_REFUSED*) : ;;
    *) fail "unexpected lock result from B: $B_RESULT" ;;
  esac
  case "$B_RESULT" in
    *B_ACQUIRED_AFTER_RELEASE*) : ;;
    *) fail "B never acquired the lock after A released: $B_RESULT" ;;
  esac
  pass "B refused while A held; B acquired after release"
fi

# ── leg 4: agent-tools battery (sqlite, git) ──────────────────────────
say "leg 4: agent-tools battery on the shared volume"
if vm "command -v sqlite3 >/dev/null"; then
  vm  "timeout 60 sqlite3 $MNT_A/agents.db 'PRAGMA busy_timeout=10000; \
       CREATE TABLE IF NOT EXISTS t(client TEXT, n INTEGER);' >/dev/null" \
    || fail "sqlite create failed from A"
  vm  "for i in \$(seq 1 50); do timeout 60 sqlite3 $MNT_A/agents.db \
         'PRAGMA busy_timeout=10000; INSERT INTO t VALUES(\"a\",'\$i');' >/dev/null || exit 1; done" \
    || fail "sqlite inserts failed from A"
  vmb "for i in \$(seq 1 50); do timeout 60 sqlite3 $MNT_B/agents.db \
         'PRAGMA busy_timeout=10000; INSERT INTO t VALUES(\"b\",'\$i');' >/dev/null || exit 1; done" \
    || fail "sqlite inserts failed from B"
  COUNT=$(vm "sqlite3 $MNT_A/agents.db 'SELECT count(*) FROM t;'" | tr -d '\r')
  [ "$COUNT" = "100" ] || fail "sqlite row count $COUNT != 100 after cross-client inserts"
  OK=$(vmb "sqlite3 $MNT_B/agents.db 'PRAGMA integrity_check;'" | tr -d '\r')
  [ "$OK" = "ok" ] || fail "sqlite integrity_check: $OK"
  pass "sqlite: 50+50 cross-client inserts, count=100, integrity ok"
else
  skip "no sqlite3 in VM"
fi
if vm "command -v git >/dev/null"; then
  vm  "cd $MNT_A && rm -rf repo && mkdir repo && cd repo && \
       timeout 120 git init -q && echo one >f && timeout 120 git add f && \
       timeout 120 git -c user.email=a@lite -c user.name=a commit -qm from-a" \
    || fail "git init/commit failed from A"
  vmb "cd $MNT_B/repo && git log --oneline | grep -c from-a" | grep -qx 1 \
    || fail "B does not see A's commit"
  vmb "cd $MNT_B/repo && echo two >g && timeout 120 git add g && \
       timeout 120 git -c user.email=b@lite -c user.name=b commit -qm from-b" \
    || fail "git commit failed from B"
  N=$(vm "cd $MNT_A/repo && git log --oneline | wc -l" | tr -d ' \r')
  [ "$N" = "2" ] || fail "A sees $N commits, expected 2"
  pass "git: cross-client init/commit/log, 2 commits visible both sides"
else
  skip "no git in VM"
fi

# ── leg 5: standalone oracle — the posture is real ────────────────────
say "leg 5: standalone oracle (no layouts, MDS-lane only, residuals moot)"
# The client must never even ASK for a layout: EXCHANGE_ID advertised a
# non-pNFS server. Any LAYOUTGET line in the log — granted, refused or
# merely received — means the posture leaked.
LG=$(grep -c "LAYOUTGET" "$MDS_LOG")
[ "${LG:-1}" = "0" ] || fail "$LG LAYOUTGET line(s) in the hub log — the client asked \
for layouts; EXCHANGE_ID did not advertise a plain NFS server"
# The 0444/F67 permission shapes (git loose objects) must stay moot in
# a posture with no placement bindings at all.
PD=$(grep -c "placement binding" "$MDS_LOG")
[ "${PD:-1}" = "0" ] || fail "placement-binding activity in standalone ($PD line(s))"
# Bytes landed in the hub's own export tree — the only tree there is.
TREE_BYTES=$(du -sk "$EXPORT_DIR" | awk '{print $1*1024}')
[ "${TREE_BYTES:-0}" -gt $(( DRILL_MIB * 1024 * 1024 / 2 )) ] \
  || fail "export tree only $TREE_BYTES bytes — where did the data go?"
# Mid-flight census: a cache-cold read holds NFS connections to the MDS
# port and nowhere else (no DS ports exist to connect to).
CENSUS='echo 3 > /proc/sys/vm/drop_caches; \
  dd if=__MNT__/shared.bin of=/dev/null bs=1M 2>/dev/null & DDPID=$!; \
  n=0; while kill -0 $DDPID 2>/dev/null; do \
    c=$(ss -Htn | awk '"'"'$5 ~ /:20490$/'"'"' | wc -l); \
    [ "$c" -gt "$n" ] && n=$c; sleep 0.1; done; \
  wait $DDPID; echo $n'
A_CONNS=$(vm  "$(printf '%s' "$CENSUS" | sed "s|__MNT__|$MNT_A|")" | tr -d ' \r')
B_CONNS=$(vmb "$(printf '%s' "$CENSUS" | sed "s|__MNT__|$MNT_B|")" | tr -d ' \r')
[ "${A_CONNS:-0}" -ge 1 ] || fail "client A held no MDS connections mid-read"
[ "${B_CONNS:-0}" -ge 1 ] || fail "client B held no MDS connections mid-read"
pass "no LAYOUTGET ever, no binding activity, tree=$TREE_BYTES bytes, \
MDS conns mid-read A=$A_CONNS B=$B_CONNS"

# ── leg 6: measurement (report, not assert) — the lite baseline ───────
say "leg 6: lite baselines from client B (agents are metadata-heavy)"
CREATE_MS=$(vmb "start=\$(date +%s%N); \
  for i in \$(seq 1 100); do : > $MNT_B/meta-\$i; rm $MNT_B/meta-\$i; done; \
  echo \$(( (\$(date +%s%N) - start) / 1000000 ))" | tr -d '\r')
STAT_MS=$(vmb "start=\$(date +%s%N); \
  for i in \$(seq 1 100); do stat -c %s $MNT_B/shared.bin >/dev/null; done; \
  echo \$(( (\$(date +%s%N) - start) / 1000000 ))" | tr -d '\r')
READ_MBS=$(vmb "echo 3 > /proc/sys/vm/drop_caches; \
  start=\$(date +%s%N); \
  dd if=$MNT_B/shared.bin of=/dev/null bs=1M 2>/dev/null; \
  el=\$(( \$(date +%s%N) - start )); echo \$(( $DRILL_MIB * 1000000000 / (el + 1) ))" | tr -d '\r')
WRITE_MBS=$(vmb "start=\$(date +%s%N); \
  dd if=/dev/zero of=$MNT_B/wtest.bin bs=1M count=$DRILL_MIB conv=fsync 2>/dev/null; \
  el=\$(( \$(date +%s%N) - start )); rm -f $MNT_B/wtest.bin; \
  echo \$(( $DRILL_MIB * 1000000000 / (el + 1) ))" | tr -d '\r')
echo "  create+unlink: 100 pairs in ${CREATE_MS} ms ($(( 200000 / (CREATE_MS + 1) )) ops/s)"
echo "  stat:          100 calls in ${STAT_MS} ms ($(( 100000 / (STAT_MS + 1) )) ops/s)"
echo "  seq read  (cold, MDS-lane): ${READ_MBS} MiB/s"
echo "  seq write (fsync, MDS-lane): ${WRITE_MBS} MiB/s"
echo "  (one hub, no layouts: these ARE the lite ceiling on this rig —"
echo "   record them; scale-up means graduating to the pNFS posture)"

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — one standalone hub served two distinct clients: coherent,"
echo " locked, tool-exercised, MDS-lane only. This is the flint-lite"
echo " premise."
echo "══════════════════════════════════════════════════════════════════"
