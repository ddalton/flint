#!/bin/bash
# nfs-proxy step 3 drills (design §4 / §8), real kernel client, real hubs.
#   bash step3-drills.sh              # all drills
#   KEEPALIVE=false bash step3-drills.sh keepalive   # CONTROL: the keepalive drill must FAIL
# Drills: restart (hub restart under a writer), proxyrestart (proxy restart
# under a writer), keepalive (an idle lock holder keeps its lock on the hub
# past the hub lease), parked (a stopped hub: the client waits, no error,
# and finishes when the hub is back), destroy (umount destroys the backend
# clients on the hubs).
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release}
# NOT /tmp: a wedged run ends in a reboot, which clears /tmp.
ROOT=$HOME/nfs-proxy-step3; MNT=/mnt/px; DIRECT=/mnt/pxd; PX=20490
HUB_LEASE=20
WHICH=${*:-restart proxyrestart keepalive parked destroy}
ok=0; bad=0
# A drill that fails can leave a HARD mount wedged in client recovery, and
# every process that stats the mount table then blocks in D state — on
# 2026-09-28 that included sshd's session setup, and the box needed a
# reboot. So: lazy-unmount and stop everything on ANY exit.
cleanup() {
  sudo umount -f -l $MNT 2>/dev/null; sudo umount -f -l $DIRECT 2>/dev/null
  pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT" 2>/dev/null
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT" 2>/dev/null
}
trap cleanup EXIT INT TERM
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
clean() { sed 's/\x1b\[[0-9;]*m//g' "$@"; }

hubconf() {  # $1 name, $2 port
  local D=$ROOT/$1; mkdir -p $D/data/exports $D/data/state
  cat > $D/mds.yaml <<Y
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "127.0.0.1", port: $2 }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $D/data/state/state.db } }
exports:
  - path: $D/data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: "info", format: text }
Y
}
hub_start() {  # $1 name, $2 tag
  sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 \
      FLINT_NFS_FSID_FROM_VOLUME=1 FLINT_NFS_STATEID_TAG=$2 FLINT_NFS_LEASE_SECS=$HUB_LEASE \
      setsid $BIN/flint-pnfs-mds --config $ROOT/$1/mds.yaml >>$ROOT/out/hub-$1.log 2>&1 < /dev/null &
  for _ in $(seq 1 50); do grep -q "server id (persistent)" <(clean $ROOT/out/hub-$1.log) && break; sleep 0.2; done
  sleep 1
}
hub_stop() { sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT/$1/"; sleep 1; }
sid() { clean $ROOT/out/hub-$1.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$'; }
proxy_start() {
  RUST_LOG=info setsid $BIN/flint-nfs-proxy --config $ROOT/proxy/config.yaml >>$ROOT/out/proxy.log 2>&1 < /dev/null &
  sleep 1
}
proxy_stop() { pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"; sleep 1; }
mnt() { sudo timeout 30 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$PX 127.0.0.1:/ $MNT; }

setup() {
  sudo umount -f -l $MNT 2>/dev/null; sudo umount -f -l $DIRECT 2>/dev/null
  proxy_stop; sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"; sleep 1
  sudo rm -rf $ROOT; mkdir -p $ROOT/out $ROOT/proxy; sudo mkdir -p $MNT $DIRECT
  hubconf ws-a 20491; hubconf ws-b 20492
  hub_start ws-a 10; hub_start ws-b 11
  cat > $ROOT/proxy/config.yaml <<Y
listen: 127.0.0.1:$PX
stateDir: $ROOT/proxy
leaseSecs: $HUB_LEASE
keepalive: ${KEEPALIVE:-true}
hubs:
  - { name: ws-a, address: "127.0.0.1:20491", serverId: $(sid ws-a), stateidTag: 10 }
  - { name: ws-b, address: "127.0.0.1:20492", serverId: $(sid ws-b), stateidTag: 11 }
identities:
  - { name: local, sources: ["127.0.0.1/32"], workspaces: ["ws-*"] }
Y
  proxy_start
}

# A writer that appends 1..N, one fsync'd line at a time. Exit 0 only if
# every write returned; the FILE is then checked for gaps on the hub.
writer() {  # $1 path, $2 N
  sudo python3 - "$1" "$2" <<'P'
import os, sys, time
p, n = sys.argv[1], int(sys.argv[2])
fd = os.open(p, os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o644)
for i in range(1, n + 1):
    os.write(fd, b"%d\n" % i); os.fsync(fd); time.sleep(0.05)
os.close(fd)
P
}
nogaps() { [ "$(sudo cat "$1" 2>/dev/null | tr '\n' ' ')" = "$(seq 1 $2 | tr '\n' ' ')" ]; }

for d in $WHICH; do
  echo "=== $d"
  setup; mnt
  case $d in
  restart)
    writer $MNT/ws-a/log 200 & W=$!
    sleep 3; hub_stop ws-a; sleep 2; hub_start ws-a 10
    wait $W; WX=$?
    check "restart: the writer never saw an error" '[ $WX = 0 ]'
    check "restart: the file on the hub has no gaps" 'nogaps $ROOT/ws-a/data/exports/log 200'
    check "restart: the proxy registered on hub A again after its restart" '[ "$(clean $ROOT/out/proxy.log | grep -c "backend client on ws-a")" -ge 2 ]'
    ;;
  proxyrestart)
    writer $MNT/ws-b/log 200 & W=$!
    sleep 3; proxy_stop; sleep 1; proxy_start
    wait $W; WX=$?
    check "proxyrestart: the writer never saw an error" '[ $WX = 0 ]'
    check "proxyrestart: the file on the hub has no gaps" 'nogaps $ROOT/ws-b/data/exports/log 200'
    check "proxyrestart: the proxy really restarted mid-write" '[ "$(clean $ROOT/out/proxy.log | grep -c "listening on")" = 2 ] && [ "$(sudo wc -l < $ROOT/ws-b/data/exports/log)" = 200 ]'
    ;;
  keepalive)
    # Holder: through the proxy, takes the lock, then stays idle.
    sudo python3 -c "
import fcntl, os, time
fd = os.open('$MNT/ws-a/lk', os.O_CREAT | os.O_RDWR)
fcntl.lockf(fd, fcntl.LOCK_EX)
open('$ROOT/out/held', 'w').close()
time.sleep(600)
" & H=$!
    for _ in $(seq 1 50); do [ -e $ROOT/out/held ] && break; sleep 0.2; done
    IDLE=$((HUB_LEASE * 3)); echo "idle ${IDLE}s (lease ${HUB_LEASE}s)"; sleep $IDLE
    # Contender: a DIRECT mount of hub A — a different client to the hub.
    sudo timeout 30 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=20491 127.0.0.1:/ $DIRECT
    GOT=$(sudo timeout 60 python3 -c "
import fcntl, os
fd = os.open('$DIRECT/lk', os.O_RDWR)
try:
    fcntl.lockf(fd, fcntl.LOCK_EX | fcntl.LOCK_NB); print('ACQUIRED')
except OSError as e: print('REFUSED', e.errno)
" 2>&1)
    echo "contender: $GOT"
    check "keepalive: the idle holder still holds its lock on the hub" 'echo "$GOT" | grep -q REFUSED'
    sudo kill $H 2>/dev/null; sudo timeout 20 umount $DIRECT || sudo umount -f -l $DIRECT
    ;;
  parked)
    echo parked-bytes | sudo tee $ROOT/ws-a/data/exports/p >/dev/null   # never read through the proxy yet
    ls $MNT/ws-a >/dev/null
    hub_stop ws-a
    (timeout 120 cat $MNT/ws-a/p > $ROOT/out/parked.out 2>$ROOT/out/parked.err; echo $? > $ROOT/out/parked.rc) &
    sleep 8
    check "parked: the reader waits (no error) while the hub is down" '[ ! -e $ROOT/out/parked.rc ]'
    check "parked: the proxy asked to wake the hub" 'clean $ROOT/out/proxy.log | grep -q "refuses connections"'
    hub_start ws-a 10
    for _ in $(seq 1 60); do [ -e $ROOT/out/parked.rc ] && break; sleep 1; done
    check "parked: the read completes once the hub is back" '[ "$(cat $ROOT/out/parked.rc 2>/dev/null)" = 0 ] && [ "$(cat $ROOT/out/parked.out)" = parked-bytes ]'
    ;;
  destroy)
    echo x | sudo tee $MNT/ws-a/d >/dev/null; echo y | sudo tee $MNT/ws-b/d >/dev/null
    sudo timeout 30 umount $MNT || sudo umount -f -l $MNT; sleep 2
    N=$(clean $ROOT/out/proxy.log | grep -c "destroyed with its downstream")
    echo "backend clients destroyed with the downstream: $N"
    check "destroy: umount destroyed the backend client on both hubs" '[ "$N" = 2 ]'
    ;;
  esac
  timeout 10 mountpoint -q $MNT && { sudo timeout 20 umount $MNT || sudo umount -f -l $MNT; }
done
proxy_stop; sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"
echo "proxy warnings:"; clean $ROOT/out/proxy.log | grep -E "WARN|ERROR" | cut -c29-200 | sort | uniq -c | sort -rn | head -12
echo "RESULT: $ok passed, $bad failed"
