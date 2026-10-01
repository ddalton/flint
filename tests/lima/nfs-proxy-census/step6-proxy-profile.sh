#!/bin/bash
# step 6, a probe from phase C's finding: the proxy spent ~0.18 ms CPU per
# metadata op (on kind), about the hub's own. Here both run as plain host
# processes (no kind), from a frame-pointer build, the host kernel mounts
# the proxy with actimeo=0 (every stat is a GETATTR), and:
#   1. CPU per 1k stat ops, proxy and hub, from /proc/<pid>/stat
#      (utime+stime) — does the kind figure reproduce?
#   2. the same through a DIRECT mount of the hub (the hub's own cost);
#   3. perf record -g on the proxy during the stat passes; the hottest
#      symbols, self and inclusive.
#   BIN=$HOME/target-prof/release bash step6-proxy-profile.sh
set -u
BIN=${BIN:-$HOME/target-prof/release}
ROOT=$HOME/nfs-proxy-prof; MNT=/mnt/pxp; PX=20590; HP=20591; FILES=5000
exec 9>$HOME/.proxy-prof.lock
flock -n 9 || { echo "another profile run holds the lock"; exit 1; }
cleanup() {
  sudo umount -f -l $MNT 2>/dev/null
  pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT" 2>/dev/null
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT" 2>/dev/null
}
trap cleanup EXIT INT TERM
clean() { sed 's/\x1b\[[0-9;]*m//g' "$@"; }
sudo rm -rf $ROOT; mkdir -p $ROOT/out $ROOT/proxy $ROOT/hub/data/exports $ROOT/hub/data/state; sudo mkdir -p $MNT

cat > $ROOT/hub/mds.yaml <<Y
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "127.0.0.1", port: $HP }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $ROOT/hub/data/state/state.db } }
exports:
  - path: $ROOT/hub/data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: "info", format: text }
Y
sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 FLINT_NFS_FSID_FROM_VOLUME=1 FLINT_NFS_STATEID_TAG=10 \
  setsid $BIN/flint-pnfs-mds --config $ROOT/hub/mds.yaml > $ROOT/out/hub.log 2>&1 < /dev/null &
for _ in $(seq 1 50); do grep -q "server id (persistent)" <(clean $ROOT/out/hub.log) && break; sleep 0.2; done
SID=$(clean $ROOT/out/hub.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$')
cat > $ROOT/proxy/config.yaml <<Y
listen: 127.0.0.1:$PX
stateDir: $ROOT/proxy
leaseSecs: 90
keepalive: true
hubs:
  - { name: ws, address: "127.0.0.1:$HP", serverId: $SID, stateidTag: 10 }
identities:
  - { name: local, sources: ["127.0.0.1/32"], workspaces: ["*"] }
Y
RUST_LOG=info setsid $BIN/flint-nfs-proxy --config $ROOT/proxy/config.yaml > $ROOT/out/proxy.log 2>&1 < /dev/null &
sleep 1
PXPID=$(pgrep -f "[f]lint-nfs-proxy --config $ROOT"); HUBPID=$(pgrep -f "^$BIN/flint-pnfs-mds --config $ROOT" | head -1)
echo "proxy pid $PXPID, hub pid $HUBPID"

mnt() {  # $1 = proxy|direct
  case $1 in
    proxy)  sudo mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,actimeo=0,port=$PX 127.0.0.1:/ $MNT && DIR=$MNT/ws ;;
    direct) sudo mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,actimeo=0,port=$HP 127.0.0.1:/ $MNT && DIR=$MNT ;;
  esac
}
mnt proxy || exit 1
sudo python3 - $MNT/ws $FILES <<'P'
import os, sys
root, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    d = os.path.join(root, f"d{i // 100:03d}")
    if i % 100 == 0:
        os.makedirs(d, exist_ok=True)
    with open(os.path.join(d, f"f{i:05d}"), "wb") as f:
        f.write(b"x" * 4096)
P
echo "seeded $(sudo find $MNT/ws -type f | wc -l) files"
sudo umount $MNT

ticks() { awk '{print $14 + $15}' /proc/$1/stat; }   # utime + stime, clock ticks
HZ=$(getconf CLK_TCK)
statpass() {  # $1 passes over $DIR -> prints ops
  sudo python3 -c "
import os
n = 0
for _ in range($1):
    for d, _, fs in os.walk('$DIR'):
        for f in fs:
            os.stat(os.path.join(d, f)); n += 1
print(n)"
}
measure() {  # $1 arm
  mnt $1 || exit 1
  statpass 1 >/dev/null   # warm the client's dentries
  local p0 h0 t0 n t1 p1 h1
  p0=$(ticks $PXPID); h0=$(ticks $HUBPID); t0=$(date +%s.%N)
  n=$(statpass 4)
  t1=$(date +%s.%N); p1=$(ticks $PXPID); h1=$(ticks $HUBPID)
  printf "%-6s ops=%s  %.0f ops/s  proxy %.1f ms CPU/1k ops  hub %.1f ms CPU/1k ops\n" $1 $n \
    $(echo "$n / ($t1 - $t0)" | bc -l) $(echo "($p1 - $p0) * 1000 / $HZ / ($n / 1000)" | bc -l) $(echo "($h1 - $h0) * 1000 / $HZ / ($n / 1000)" | bc -l)
  sudo umount $MNT
}
echo "== CPU per op (4 stat passes of $FILES files, after a warm pass)"
for r in 1 2 3; do measure direct; measure proxy; done | tee $ROOT/out/cpu.txt

echo "== perf record on the proxy during 6 stat passes"
mnt proxy || exit 1
statpass 1 >/dev/null
sudo perf record -F 1999 -g --call-graph fp -p $PXPID -o $ROOT/out/perf.data -- sleep 30 > $ROOT/out/perf-record.log 2>&1 &
PR=$!; sleep 1
statpass 12 >/dev/null
wait $PR
sudo umount $MNT
sudo chown $USER $ROOT/out/perf.data
perf report -i $ROOT/out/perf.data --no-children --sort symbol --stdio 2>/dev/null | grep -v "^#" | grep -v "^$" | head -45 > $ROOT/out/self.txt
perf report -i $ROOT/out/perf.data --children --sort symbol --stdio 2>/dev/null | grep -E "^ +[0-9]" | head -45 > $ROOT/out/children.txt
echo "-- self (top)"; cut -c1-160 $ROOT/out/self.txt | grep -E "^ +[0-9]" | head -30
echo "-- inclusive (top)"; cut -c1-160 $ROOT/out/children.txt | head -30
echo "-- proxy log lines during the run: $(wc -l < $ROOT/out/proxy.log)"
