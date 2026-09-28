#!/bin/bash
# nfs-proxy step 1, live: a real flint lite hub with H1 + H2 on, and the
# control arm with both off. What reaches the wire: the fsid in GETATTR
# replies (H1: the hub's persistent server id), and other[8..12] of the
# OPEN stateid (H2: the tag). Plus: a malformed tag must refuse to start.
# Decode the pcaps with tshark (-d tcp.port==20490,rpc).
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release/flint-pnfs-mds}
PORT=20490; TAG=659918   # 0x000A11CE
ROOT=/tmp/nfs-proxy-step1; sudo umount -f /mnt/s1 2>/dev/null
sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT" ; sleep 1
sudo rm -rf $ROOT; mkdir -p $ROOT/out; sudo mkdir -p /mnt/s1

conf() {  # $1 = arm dir
  mkdir -p $1/data/exports $1/data/state
  cat > $1/mds.yaml <<Y
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "127.0.0.1", port: $PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $1/data/state/state.db } }
exports:
  - path: $1/data/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [ { network: 0.0.0.0/0, permissions: rw } ]
logging: { level: "info", format: text }
Y
}

arm() {  # $1 = name, rest = env assignments
  local name=$1; shift; local D=$ROOT/$name
  conf $D
  sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 "$@" \
      setsid $BIN --config $D/mds.yaml >$D/hub.log 2>&1 < /dev/null &
  sleep 3
  sudo ss -ltn | grep -q ":$PORT " || { echo "$name: HUB DID NOT START"; tail -5 $D/hub.log; return 1; }
  sudo tcpdump -i lo -s 0 -w $ROOT/out/$name.pcap port $PORT >/dev/null 2>&1 & local CAP=$!; sleep 1
  sudo mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,port=$PORT 127.0.0.1:/ /mnt/s1
  echo x | sudo tee /mnt/s1/f >/dev/null; stat -c "$name: st_dev seen by the client = %d" /mnt/s1/f
  python3 -c "open('/mnt/s1/f').read()"
  sudo umount /mnt/s1; sleep 1; sudo kill $CAP; wait $CAP 2>/dev/null
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $D"; sleep 1
  echo "$name: server id in log: $(sed 's/\x1b\[[0-9;]*m//g' $D/hub.log | grep -o 'server id (persistent): [0-9]*' | head -1)"
  echo "$name: export st_dev on the host: $(stat -c %d $D/data/exports)"
  sed 's/\x1b\[[0-9;]*m//g' $D/hub.log | grep -E "export fsid =|stateid tag" | cut -c29-200
}

arm h1h2-on FLINT_NFS_FSID_FROM_VOLUME=1 FLINT_NFS_STATEID_TAG=$TAG
arm control
# A malformed tag must refuse to start, not serve untagged stateids.
D=$ROOT/badtag; conf $D
sudo env FLINT_NFS_STATEID_TAG=0xA11CE timeout 20 $BIN --config $D/mds.yaml >$D/hub.log 2>&1 < /dev/null
echo "badtag: exit=$? $(sed 's/\x1b\[[0-9;]*m//g' $D/hub.log | grep -o 'FLINT_NFS_STATEID_TAG=.*' | head -1)"
sudo ss -ltn | grep -c ":$PORT " | sed 's/^/listeners left: /'
sudo chmod -R a+r $ROOT/out; ls $ROOT/out
