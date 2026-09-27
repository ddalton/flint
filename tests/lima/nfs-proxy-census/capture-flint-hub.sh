#!/bin/bash
# Census step 0b, part 2: within-workspace COMPOUND shapes against a REAL
# flint lite hub (flint-pnfs-mds, mode: standalone, config as
# lite_operator::render::mds_yaml renders it, no tier), kernel client on
# the same host. knfsd shapes do not transfer where they depend on what
# the SERVER advertises (xattr_support), so these are re-captured here.
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release/flint-pnfs-mds}
W=/tmp/census-flint; OUT=$W/out; PORT=20490
sudo umount -f /mnt/fh 2>/dev/null; sudo pkill -f "flint-pnfs-mds --config $W" 2>/dev/null; sleep 1
sudo rm -rf $W; mkdir -p $OUT; sudo mkdir -p $W/data/exports $W/data/state /mnt/fh
cat > $W/mds.yaml <<Y
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind:
    address: "127.0.0.1"
    port: $PORT
  layout:
    type: file
    stripeSize: 8388608
    policy: stripe
  dataServers: []
  state:
    backend: sqlite
    config:
      path: $W/data/state/state.db
exports:
  - path: $W/data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access:
      - network: 0.0.0.0/0
        permissions: rw
logging:
  level: "info"
  format: text
Y
start() { sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 \
    setsid $BIN --config $W/mds.yaml >>$W/hub.log 2>&1 < /dev/null & sleep 3
  sudo ss -ltnp | grep -q ":$PORT " && echo "hub listening" || { echo "HUB DID NOT START"; tail -20 $W/hub.log; exit 1; }; }
cap() { sudo tcpdump -i lo -s 0 -w $OUT/$1.pcap port $PORT >/dev/null 2>&1 & CAP=$!; sleep 1; }
stop() { sleep 1; sudo kill $CAP; wait $CAP 2>/dev/null; }
start
O="-o nfsvers=4.2,proto=tcp,hard,port=$PORT"
cap h1-mount;        sudo mount -t nfs4 $O 127.0.0.1:/ /mnt/fh; stop
cap h2-ls-la;        sudo mkdir -p /mnt/fh/sub; echo hello | sudo tee /mnt/fh/f >/dev/null; sudo chmod -R 777 /mnt/fh; ls -la /mnt/fh /mnt/fh/sub >/dev/null; stop
cap h3-rename;       mv /mnt/fh/f /mnt/fh/f2; mv /mnt/fh/f2 /mnt/fh/f; stop
cap h4-open-lock;    python3 - <<'PY' &
import fcntl, time, os
f = open('/mnt/fh/f', 'r+'); fcntl.lockf(f, fcntl.LOCK_EX, 1, 0)
open('/tmp/census-flint/out/locked', 'w').close()
while not os.path.exists('/tmp/census-flint/out/restarted'): time.sleep(0.5)
time.sleep(5)
try:
    f.seek(0); f.write('y'); f.flush(); os.fsync(f.fileno()); print('write after hub restart: ok')
except OSError as e: print('write after hub restart:', e)
fcntl.lockf(f, fcntl.LOCK_UN, 1, 0); f.close(); print('unlock+close: ok')
PY
LOCKER=$!
for i in $(seq 1 40); do [ -e $OUT/locked ] && break; sleep 0.5; done; stop
# The design's §2 claim: clientids/stateids/locks persist, so a hub restart
# is BADSESSION -> CREATE_SESSION on the SAME clientid, with no reclaim.
# RESTART=0 is the control arm: the same open/lock/write/unlock/close with no restart.
if [ "${RESTART:-1}" = 1 ]; then
cap h5-hub-restart;  sudo pkill -TERM -f "flint-pnfs-mds --config $W"; sleep 2; start; touch $OUT/restarted; wait $LOCKER; stop
else
cap h5-no-restart;   touch $OUT/restarted; wait $LOCKER; stop
fi
cap h6-umount;       sudo umount /mnt/fh; stop
sudo pkill -TERM -f "flint-pnfs-mds --config $W"; sleep 1
sudo chmod -R a+r $OUT $W/hub.log; ls $OUT; echo "--- hub WARN/ERROR after the lock:"; sed "s/\x1b\[[0-9;]*m//g" $W/hub.log | grep -E "WARN|ERROR" | grep -v -i "keytab\|gRPC" | cut -c29-200 | tail -8
