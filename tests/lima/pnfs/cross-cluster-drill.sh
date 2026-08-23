#!/usr/bin/env bash
#
# Cross-cluster sharing drill — two DISTINCT NFS clients on one volume.
#
# The multi-cluster plan's Phase 0: prove that two independent kernel
# NFS clients (stand-ins for pods in two K8s compute clusters) can share
# one flint volume with the semantics the agent-fleet use case needs:
#
#   leg 1  distinctness  — the two mounts are different superblocks,
#          i.e. different nfs_client instances with different client
#          identities. Without this every later leg is VACUOUS: two
#          mounts of the same export from one kernel share a superblock
#          and a page cache, and "coherence" between them proves nothing.
#   leg 2  close-to-open — A writes+closes, B open+reads byte-identical
#          (and never zeros); then the reverse direction.
#   leg 3  cross-client lock exclusion — a POSIX byte-range lock held
#          via mount A must refuse a non-blocking attempt via mount B,
#          and succeed after release. Distinct superblocks mean the only
#          arbiter is the SERVER (there is no shared VFS lock state).
#   leg 4  agent-tools battery — sqlite (locks in anger) and git run
#          against the shared volume from both clients. SKIPs loudly if
#          a tool is missing in the VM.
#   leg 5  DS-direct oracle — both DS export trees grew (real striping)
#          and BOTH clients hold TCP connections to the DS ports. The
#          F68 lesson: connection topology is the truth; a client that
#          can reach the MDS but not the DSes silently routes every
#          byte through the MDS.
#   leg 6  metadata-rate measurement (not an assertion) — create/unlink
#          and stat rates from client B. Agents are metadata-heavy and
#          delegations are off, so this number decides workload fit.
#   leg 7  the INVERSE of leg 1: a third client wearing client A's
#          hostname (same co_ownerid, own verifier) must not be handed
#          a lock A still holds. Two pods in two workload clusters can
#          trivially share a hostname, and RFC 8881 reads that as "the
#          client rebooted" — correct per spec, fatal between machines.
#
# Client B's distinctness is manufactured the only honest way one VM
# allows: the mount runs in its own network namespace (own nfs_net, own
# transports) under its own UTS hostname (own co_ownerid). The netns
# reaches the host servers via a veth pair + MASQUERADE.
#
# Servers run on the macOS host (smoke.sh topology); the Lima VM is the
# client. KEEP=1 leaves the stack and mounts standing for inspection.
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
DS1_PORT=20491
DS2_PORT=20492
DS1_EXPORT=/tmp/flint-pnfs-ds1
DS2_EXPORT=/tmp/flint-pnfs-ds2
MDS_EXPORT_DIR=/tmp/flint-pnfs-mds-exports

MNT_A=/mnt/cc-a
MNT_B=/mnt/cc-b
NETNS=ccb
VETH_HOST=ccb0
VETH_NS=ccb1
NS_NET=10.99.77.0/30
NS_GW=10.99.77.1
NS_IP=10.99.77.2
# A THIRD client, used only by leg 7: its own netns, but client A's
# hostname. Two pods in two clusters that happen to agree on a name.
MNT_C=/mnt/cc-c
NETNS_C=cca
VETH_C_HOST=cca0
VETH_C_NS=cca1
NS_C_NET=10.99.78.0/30
NS_C_GW=10.99.78.1
NS_C_IP=10.99.78.2
DRILL_MIB=64

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
skip() { echo "  △ SKIP: $*"; }
fail() { echo "  ✗ FAIL: $*"; FAILED=1; exit 1; }

vm()   { limactl shell "$LIMA_VM" -- sudo sh -c "$1"; }
# Run a command as client B: inside the netns. (The UTS namespace only
# matters at MOUNT time — the client identity string is captured when
# the transport is created. Later I/O through $MNT_B uses B's superblock
# and transports whichever namespace the calling process is in, but we
# keep B's processes in the netns anyway so its DS connections are
# observable there with ss.)
vmb()  { limactl shell "$LIMA_VM" -- sudo ip netns exec "$NETNS" sh -c "$1"; }

cleanup() {
  set +e
  if [ "${KEEP:-0}" = "1" ]; then
    echo "KEEP=1 — leaving servers, mounts and netns standing"
    return
  fi
  # Kill the lock holder first: a python process holding a lock on a
  # mount whose server is about to die D-states the umount.
  vm "pkill -f cc-alias-a.py 2>/dev/null; true" 2>/dev/null
  vm "umount -lf $MNT_C 2>/dev/null; umount -lf $MNT_B 2>/dev/null; \
      umount -lf $MNT_A 2>/dev/null; \
      ip netns del $NETNS 2>/dev/null; ip link del $VETH_HOST 2>/dev/null; \
      ip netns del $NETNS_C 2>/dev/null; ip link del $VETH_C_HOST 2>/dev/null; \
      iptables -t nat -D POSTROUTING -s $NS_NET -j MASQUERADE 2>/dev/null; \
      iptables -t nat -D POSTROUTING -s $NS_C_NET -j MASQUERADE 2>/dev/null" \
      2>/dev/null
  for n in mds ds1 ds2; do
    [ -f "$PIDFILE_DIR/flint-pnfs-$n.pid" ] && \
      kill "$(cat "$PIDFILE_DIR/flint-pnfs-$n.pid")" 2>/dev/null
    rm -f "$PIDFILE_DIR/flint-pnfs-$n.pid"
  done
  pkill -9 -f flint-pnfs-mds 2>/dev/null
  pkill -9 -f flint-pnfs-ds  2>/dev/null
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " cross-cluster drill — two distinct NFS clients, one flint volume"
echo "══════════════════════════════════════════════════════════════════"

# ── pre-flight ─────────────────────────────────────────────────────────
for bin in flint-pnfs-mds flint-pnfs-ds; do
  [ -x "$BIN_DIR/$bin" ] || fail "missing $BIN_DIR/$bin — run 'make build-pnfs'"
done
command -v limactl >/dev/null || fail "limactl not found (brew install lima)"
limactl list --quiet 2>/dev/null | grep -qx "$LIMA_VM" \
  || fail "Lima VM '$LIMA_VM' not running — run 'make lima-up'"
for tool in ip iptables findmnt ss md5sum unshare nsenter; do
  vm "command -v $tool >/dev/null" \
    || fail "VM lacks '$tool' — the netns client needs it"
done

# Fresh world, honest byte counters.
cleanup
rm -rf "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
mkdir -p "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"
chmod 0777 "$DS1_EXPORT" "$DS2_EXPORT" "$MDS_EXPORT_DIR"

# ── start MDS + 2 DSes on the host (smoke.sh topology) ────────────────
say "starting MDS + 2 DSes"
PNFS_MODE=mds nohup "$BIN_DIR/flint-pnfs-mds" --config "$CFG_DIR/mds.yaml" \
  >"$LOG_DIR/flint-pnfs-mds.log" 2>&1 &
echo $! >"$PIDFILE_DIR/flint-pnfs-mds.pid"
sleep 1
kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-mds.pid")" 2>/dev/null \
  || { tail -20 "$LOG_DIR/flint-pnfs-mds.log"; fail "MDS died on startup"; }
for n in 1 2; do
  PNFS_MODE=ds nohup "$BIN_DIR/flint-pnfs-ds" --config "$CFG_DIR/ds$n.yaml" \
    >"$LOG_DIR/flint-pnfs-ds$n.log" 2>&1 &
  echo $! >"$PIDFILE_DIR/flint-pnfs-ds$n.pid"
done
sleep 2
for n in 1 2; do
  kill -0 "$(cat "$PIDFILE_DIR/flint-pnfs-ds$n.pid")" 2>/dev/null \
    || { tail -20 "$LOG_DIR/flint-pnfs-ds$n.log"; fail "DS $n died on startup"; }
done
pass "servers up (mds:$MDS_PORT ds:$DS1_PORT,$DS2_PORT)"

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
# The UTS unshare + hostname happen in the SAME process as the mount so
# the new nfs_client captures 'cc-cluster-b' as its co_ownerid — that,
# plus the distinct nfs_net, is what makes B a different client.
vm "mkdir -p $MNT_B"
# The mount is itself the connectivity probe — bounded by timeout so a
# black-holed route fails loudly instead of hanging the drill.
# nsenter --net, NOT ip netns exec: the latter clones a private MOUNT
# namespace (for per-netns /etc bind-mounts), so the NFS mount would
# evaporate with it. --net enters only the network namespace; the mount
# lands in the shared mount table while the kernel pins the mount's
# transports to the netns it was created in — B's traffic stays in ccb.
vm "nsenter --net=/var/run/netns/$NETNS unshare --uts sh -c ' \
      hostname cc-cluster-b && \
      timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
        $HOST_IP:/ $MNT_B'" || fail "client B mount failed (netns route to host?)"
pass "client B mounted at $MNT_B from netns $NETNS as cc-cluster-b"

# ── leg 1: distinctness — different superblocks or the drill is void ──
say "leg 1: the two mounts must be different superblocks"
SB_A=$(vm "findmnt -no MAJ:MIN $MNT_A" | tr -d ' \r')
SB_B=$(vm "findmnt -no MAJ:MIN $MNT_B" | tr -d ' \r')
[ -n "$SB_A" ] && [ -n "$SB_B" ] || fail "could not read superblock ids"
[ "$SB_A" != "$SB_B" ] \
  || fail "BOTH MOUNTS SHARE SUPERBLOCK $SB_A — one nfs_client, one page \
cache; every coherence leg below would pass vacuously. The netns/UTS \
split did not produce a distinct client identity."
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
  skip "no python3 in VM — lock leg needs fcntl; install python3 to run it"
else
  # A grabs an exclusive lock and holds it for 8s. setsid + closed stdin
  # is load-bearing: the holder must survive its ssh session closing, or
  # the lock releases early and B's refusal leg reports a false failure.
  vm "rm -f /tmp/cc-lock-a.log; \
      cat >/tmp/cc-lock-a.py <<'PY'
import fcntl, time
f = open('$MNT_A/shared.bin', 'r+b')
fcntl.lockf(f, fcntl.LOCK_EX, 4096, 0)
print('A_LOCKED', flush=True)
time.sleep(8)
fcntl.lockf(f, fcntl.LOCK_UN, 4096, 0)
print('A_UNLOCKED', flush=True)
PY
      setsid python3 /tmp/cc-lock-a.py >/tmp/cc-lock-a.log 2>&1 </dev/null & \
      for i in \$(seq 50); do grep -q A_LOCKED /tmp/cc-lock-a.log 2>/dev/null && break; sleep 0.2; done; \
      grep -c A_LOCKED /tmp/cc-lock-a.log" | grep -q '^1' \
    || fail "client A never acquired its lock"
  # B must be refused while A holds, then succeed after A releases.
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
       timeout 120 git -c user.email=a@cc -c user.name=a commit -qm from-a" \
    || fail "git init/commit failed from A"
  vmb "cd $MNT_B/repo && git log --oneline | grep -c from-a" | grep -qx 1 \
    || fail "B does not see A's commit"
  vmb "cd $MNT_B/repo && echo two >g && timeout 120 git add g && \
       timeout 120 git -c user.email=b@cc -c user.name=b commit -qm from-b" \
    || fail "git commit failed from B"
  N=$(vm "cd $MNT_A/repo && git log --oneline | wc -l" | tr -d ' \r')
  [ "$N" = "2" ] || fail "A sees $N commits, expected 2"
  pass "git: cross-client init/commit/log, 2 commits visible both sides"
else
  skip "no git in VM"
fi

# ── leg 5: DS-direct oracle — striping is real and BOTH clients did it ─
say "leg 5: connection topology + bytes-on-disk"
DS1_BYTES=$(du -sk "$DS1_EXPORT" | awk '{print $1*1024}')
DS2_BYTES=$(du -sk "$DS2_EXPORT" | awk '{print $1*1024}')
[ "${DS1_BYTES:-0}" -gt 0 ] && [ "${DS2_BYTES:-0}" -gt 0 ] \
  || fail "a DS export is empty (ds1=$DS1_BYTES ds2=$DS2_BYTES) — data did not stripe"
# flint grants every layout with return_on_close, and the kernel frees
# DS clients (DESTROY_SESSION + disconnect, ~100ms) the moment the last
# layout segment is returned — so DS connections exist only WHILE a file
# is being actively read/written. Sample the census mid-flight: start a
# cache-cold read in the background and take the max conn count seen
# while it runs.
DS_CENSUS='echo 3 > /proc/sys/vm/drop_caches; \
  dd if=__MNT__/shared.bin of=/dev/null bs=1M 2>/dev/null & DDPID=$!; \
  n=0; while kill -0 $DDPID 2>/dev/null; do \
    c=$(ss -Htn | awk '"'"'$5 ~ /:2049[12]$/'"'"' | wc -l); \
    [ "$c" -gt "$n" ] && n=$c; sleep 0.1; done; \
  wait $DDPID; echo $n'
A_DS_CONNS=$(vm  "$(printf '%s' "$DS_CENSUS" | sed "s|__MNT__|$MNT_A|")" | tr -d ' \r')
B_DS_CONNS=$(vmb "$(printf '%s' "$DS_CENSUS" | sed "s|__MNT__|$MNT_B|")" | tr -d ' \r')
[ "${A_DS_CONNS:-0}" -ge 1 ] || fail "client A holds no DS connections — MDS-only I/O (the F68 shape)"
[ "${B_DS_CONNS:-0}" -ge 1 ] || fail "client B holds no DS connections — MDS-only I/O (the F68 shape)"
pass "ds1=$DS1_BYTES ds2=$DS2_BYTES bytes on disk; DS conns A=$A_DS_CONNS B=$B_DS_CONNS"

# ── leg 6: metadata-rate measurement (report, not assert) ─────────────
say "leg 6: metadata rates from client B (agents are metadata-heavy)"
CREATE_MS=$(vmb "start=\$(date +%s%N); \
  for i in \$(seq 1 100); do : > $MNT_B/meta-\$i; rm $MNT_B/meta-\$i; done; \
  echo \$(( (\$(date +%s%N) - start) / 1000000 ))" | tr -d '\r')
STAT_MS=$(vmb "start=\$(date +%s%N); \
  for i in \$(seq 1 100); do stat -c %s $MNT_B/shared.bin >/dev/null; done; \
  echo \$(( (\$(date +%s%N) - start) / 1000000 ))" | tr -d '\r')
echo "  create+unlink: 100 pairs in ${CREATE_MS} ms ($(( 200000 / (CREATE_MS + 1) )) ops/s)"
echo "  stat:          100 calls in ${STAT_MS} ms ($(( 100000 / (STAT_MS + 1) )) ops/s)"
echo "  (delegations off: every open is an MDS round trip — these numbers"
echo "   decide agent-workload fit; record them, do not tune around them here)"

# ── leg 7: client-identity COLLISION across "clusters" ────────────────
say "leg 7: a third client wearing A's hostname must not silently take A's state"
# Legs 1-6 prove two clients with DISTINCT identities coexist. This leg
# is their inverse, and it is the one that matters once the clusters are
# real: co_ownerid is derived from the client's hostname, and two pods
# in two workload clusters can trivially have the same one.
#
# The server implements RFC 8881 §18.35 faithfully — same co_ownerid,
# fresh verifier, same principal is "the client rebooted" (Case 5,
# state/client.rs:602), so the newcomer's CREATE_SESSION DESTROYS the
# incumbent's state. Correct per the spec, and catastrophic where the
# two are different machines that merely agree on a hostname. Nothing in
# docs/identity-contract.md covers it: that governs VOLUME identity, not
# the EXCHANGE_ID client owner.
#
# The assertion is about the CONSEQUENCE rather than the wire: a lock A
# holds must not evaporate because someone else turned up with A's name.
if ! vm "command -v python3 >/dev/null"; then
  skip "no python3 in VM — the collision leg needs fcntl"
else
  HOST_A=$(vm "hostname" | tr -d '\r')
  echo "  client A's identity is '$HOST_A'; the newcomer will claim the same"
  vm "ip netns del $NETNS_C 2>/dev/null; ip link del $VETH_C_HOST 2>/dev/null; true"
  vm "ip netns add $NETNS_C && \
      ip link add $VETH_C_HOST type veth peer name $VETH_C_NS && \
      ip link set $VETH_C_NS netns $NETNS_C && \
      ip addr add $NS_C_GW/30 dev $VETH_C_HOST && ip link set $VETH_C_HOST up && \
      ip netns exec $NETNS_C ip addr add $NS_C_IP/30 dev $VETH_C_NS && \
      ip netns exec $NETNS_C ip link set $VETH_C_NS up && \
      ip netns exec $NETNS_C ip link set lo up && \
      ip netns exec $NETNS_C ip route add default via $NS_C_GW && \
      { iptables -C FORWARD -s $NS_C_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -s $NS_C_NET -j ACCEPT; } && \
      { iptables -C FORWARD -d $NS_C_NET -j ACCEPT 2>/dev/null || iptables -I FORWARD -d $NS_C_NET -j ACCEPT; } && \
      { iptables -t nat -C POSTROUTING -s $NS_C_NET -j MASQUERADE 2>/dev/null || \
        iptables -t nat -A POSTROUTING -s $NS_C_NET -j MASQUERADE; }" \
    || fail "netns plumbing for the colliding client failed"

  # A takes an exclusive lock and holds it well past the collision.
  # setsid + closed stdin so it outlives the ssh session, as in leg 3.
  vm "rm -f /tmp/cc-alias-a.log; \
      cat >/tmp/cc-alias-a.py <<'PY'
import fcntl, time
f = open('$MNT_A/shared.bin', 'r+b')
fcntl.lockf(f, fcntl.LOCK_EX, 4096, 8192)
print('A_LOCKED', flush=True)
time.sleep(40)
PY
      setsid python3 /tmp/cc-alias-a.py >/tmp/cc-alias-a.log 2>&1 </dev/null & \
      for i in \$(seq 50); do grep -q A_LOCKED /tmp/cc-alias-a.log 2>/dev/null && break; sleep 0.2; done; \
      grep -c A_LOCKED /tmp/cc-alias-a.log" | grep -q '^1' \
    || fail "client A never acquired the lock this leg is about"
  pass "client A holds an exclusive lock on bytes 4096-12287"

  # The newcomer mounts wearing A's hostname: same co_ownerid, its own
  # boot verifier — the exact shape of two same-named pods in two
  # clusters.
  vm "mkdir -p $MNT_C"
  vm "nsenter --net=/var/run/netns/$NETNS_C unshare --uts sh -c ' \
        hostname $HOST_A && \
        timeout 30 mount -t nfs4 -o minorversion=1,proto=tcp,port=$MDS_PORT \
          $HOST_IP:/ $MNT_C'" \
    || fail "the colliding client could not mount (netns route to host?)"
  pass "a second client mounted under the SAME hostname '$HOST_A'"

  # ANTI-VACUITY, the same guard leg 1 applies to A-vs-B. If the kernel
  # folded C into A's nfs_client, C's fcntl would be arbitrated LOCALLY
  # and a refusal below would say nothing whatever about the server. The
  # netns split should prevent it — assert rather than assume.
  SB_A2=$(vm "findmnt -no MAJ:MIN $MNT_A" | tr -d ' \r')
  SB_C=$(vm "findmnt -no MAJ:MIN $MNT_C" | tr -d ' \r')
  [ -n "$SB_C" ] || fail "could not read the newcomer's superblock"
  [ "$SB_A2" != "$SB_C" ] \
    || fail "the newcomer SHARES superblock $SB_C with client A — the kernel \
folded them into one nfs_client, so any refusal below is the LOCAL lock \
manager and this leg would pass vacuously. The collision was not \
manufactured; do not read the result as a server property."
  pass "the newcomer is a distinct nfs_client ($SB_A2 vs $SB_C) — refusals below are the SERVER's"

  # SECOND anti-vacuity guard, and the one that decides whether this leg
  # tests what it says. Linux does not derive co_ownerid from the
  # hostname alone, so a kernel that disambiguates by source address
  # would give the newcomer its OWN identity and there would be no
  # collision to survive. The server says which happened: a genuine
  # collision is EXCHANGE_ID case 5 ("client reboot detected"), because
  # same owner + fresh verifier IS a reboot as far as RFC 8881 knows.
  grep -q "case 5 (client reboot detected)" "$LOG_DIR/flint-pnfs-mds.log" \
    || { grep -oE "EXCHANGE_ID: (new client [0-9]+|case [0-9]+ \([^)]*\))" \
           "$LOG_DIR/flint-pnfs-mds.log" | sort | uniq -c; \
         fail "the server never saw an owner collision — the newcomer was \
given its own identity, so there was nothing for it to take and the \
result below means nothing. The collision must be re-manufactured (try \
the nfs4_unique_id module parameter) before this leg can be believed."; }
  pass "the server logged EXCHANGE_ID case 5 — the co_ownerid genuinely collided"

  C_RESULT=$(vm "nsenter --net=/var/run/netns/$NETNS_C python3 - <<'PY'
import fcntl, errno
f = open('$MNT_C/shared.bin', 'r+b')
try:
    fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, 4096, 8192)
    print('C_TOOK_A_LOCK_A_STILL_HOLDS')
except OSError as e:
    print('C_REFUSED' if e.errno in (errno.EAGAIN, errno.EACCES) else 'C_ERRNO_%d' % e.errno)
PY" | tr -d '\r')
  echo "  newcomer's result: $C_RESULT"

  # THE ORACLE, REWRITTEN 2026-08-23, and the reason is worth recording
  # because it is the exact anti-pattern this repo keeps re-learning.
  #
  # This leg used to score C_REFUSED as PASS: "the incumbent's lock
  # SURVIVED". It did survive — because of a BUG. The case-5 cascade tore
  # down A's sessions, stateids, delegations and client record but NOT its
  # locks, and could not: the session handler held no reference to the lock
  # table. So what refused C was a PHANTOM — a lock naming a clientid the
  # server had already destroyed, which A could never LOCKU and no reaper
  # could ever collect (the only reaper iterates expired LEASES, and
  # remove_client had dropped A's lease). Persisted and re-seeded at
  # startup, it denied that range to every client in every cluster forever.
  #
  # The leg's two anti-vacuity guards (distinct superblock, case 5 logged)
  # could not see this: neither asks whether the surviving lock still
  # belongs to a client the server can RESOLVE. A leg that cannot tell
  # "legitimately held" from "unreapable phantom" reads a bug as a pass.
  #
  # WHAT IS ACTUALLY ACHIEVABLE. RFC 8881 §18.35.5 case 5 is mandatory:
  # same co_ownerid + fresh verifier + same principal IS "the client
  # rebooted", and the server MUST discard the incumbent's state. It is
  # not permitted to disambiguate by source address. So C acquiring is
  # CORRECT — and identity collision really is state theft. That is a
  # DEPLOYMENT hazard, fixed by unique client names
  # (docs/flint-lite-for-agent-fleets.md, "One hub, many clusters"), not a
  # server bug the server is allowed to fix.
  #
  # What the server DOES owe, and what this leg now tests:
  #   1. the incumbent's state is released COMPLETELY and REAPABLY, so no
  #      phantom outlives it; and
  #   2. the range is usable afterwards by a client that can be resolved.
  case "$C_RESULT" in
    C_TOOK_A_LOCK_A_STILL_HOLDS)
      pass "the newcomer acquired the range — RFC 8881 case 5, the incumbent's state was discarded"
      echo "  this is CORRECT per spec and CATASTROPHIC between machines:"
      echo "  two same-named pods in two clusters WILL take each other's locks."
      echo "  The fix is unique client identity per consumer (nfs4_unique_id),"
      echo "  not anything the server is permitted to do." ;;
    C_REFUSED)
      fail "the newcomer was REFUSED after a genuine case-5 collision. The \
server destroyed A's client record (case 5 above) but something still holds \
A's range — which is the unreapable-phantom shape: a lock naming a clientid \
the server can no longer resolve, which A cannot release and no reaper can \
collect, denying this range to every client in every cluster permanently. \
Check the case-5 cascade releases locks (session.rs handle_create_session \
must hold a LockManager reference)." ;;
    *)
      fail "the colliding client failed in an unexpected way ($C_RESULT) — neither refusal nor takeover; investigate before drawing any conclusion" ;;
  esac

  # NO PHANTOM SURVIVED. The decisive check, and the one the old oracle
  # lacked: after the incumbent lets go, the range must be usable again by
  # a client the server can resolve. C is that client — distinct
  # nfs_client (asserted above), its own live session.
  #
  # Pre-fix this FAILS, and fails for the right reason: A's orphaned lock
  # is still in the table, A's LOCKU cannot reach it (its clientid is
  # gone), so the range is refused forever. That differential is what
  # makes this assertion worth making.
  vm "pkill -f cc-alias-a.py 2>/dev/null; true"
  sleep 2
  RECLAIM=$(vm "nsenter --net=/var/run/netns/$NETNS_C python3 - <<'PY'
import fcntl, errno
f = open('$MNT_C/shared.bin', 'r+b')
try:
    fcntl.lockf(f, fcntl.LOCK_UN, 4096, 8192)
except OSError:
    pass
try:
    fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB, 4096, 8192)
    print('RANGE_USABLE')
    fcntl.lockf(f, fcntl.LOCK_UN, 4096, 8192)
except OSError as e:
    print('RANGE_DENIED_%d' % e.errno)
PY" | tr -d '\r')
  [ "$RECLAIM" = "RANGE_USABLE" ] \
    || fail "after the incumbent released, the range is STILL refused \
($RECLAIM) — a lock outlived the client that held it, and nothing left can \
reap it. This is the permanent-denial shape, not a transient."
  pass "no phantom outlived the collision — the range is usable by a resolvable client"

  # A crash in the incumbent would mean the takeover was not merely
  # silent but destructive to it.
  if vm "grep -q Traceback /tmp/cc-alias-a.log 2>/dev/null"; then
    vm "tail -3 /tmp/cc-alias-a.log"
    fail "the incumbent's lock holder CRASHED during the collision"
  fi
  pass "the incumbent did not crash"
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " PASS — two distinct clients shared one volume: coherent, locked,"
echo " tool-exercised, DS-direct. This is the multi-cluster premise."
echo
echo " And a third client sharing a hostname DID take the first one's"
echo " lock — correctly, per RFC 8881 case 5, which is why unique client"
echo " identity is a deployment REQUIREMENT and not a recommendation."
echo " What the server owes it discharged: nothing was left behind that"
echo " could not be reaped."
echo "══════════════════════════════════════════════════════════════════"
