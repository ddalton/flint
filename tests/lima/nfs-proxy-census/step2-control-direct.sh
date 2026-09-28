#!/bin/bash
# Control arm for step2-e2e.sh: the SAME two hubs (H1 + H2 on), each
# mounted DIRECTLY — no proxy. An outcome that also appears here is the
# hub's (or the client's), not the proxy's.
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release}
ROOT=/tmp/nfs-proxy-step2c
for m in /mnt/dA /mnt/dB; do sudo umount -f $m 2>/dev/null; sudo mkdir -p $m; done
sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"; sleep 1
sudo rm -rf $ROOT; mkdir -p $ROOT/out
hub() {  # $1 name, $2 port, $3 tag
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
  sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 \
      FLINT_NFS_FSID_FROM_VOLUME=1 FLINT_NFS_STATEID_TAG=$3 \
      setsid $BIN/flint-pnfs-mds --config $D/mds.yaml >$ROOT/out/hub-$1.log 2>&1 < /dev/null &
}
hub ws-a 20501 10; hub ws-b 20502 11; sleep 3
sudo mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=20501 127.0.0.1:/ /mnt/dA
sudo mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=20502 127.0.0.1:/ /mnt/dB
echo hello-a | sudo tee /mnt/dA/f >/dev/null
echo "direct cross-mount mv: $(sudo timeout 10 mv /mnt/dA/f /mnt/dB/g 2>&1; echo exit=$?)"
echo "setfacl-free chmod on B: $(sudo timeout 10 sh -c 'echo x > /mnt/dB/h && chmod 600 /mnt/dB/h' 2>&1; echo exit=$?)"
echo "nfs4_acl xattr copy on B: $(sudo timeout 10 python3 -c "
import os
v=os.getxattr('/mnt/dA/', 'system.nfs4_acl')
os.setxattr('/mnt/dB/h', 'system.nfs4_acl', v)
" 2>&1 | tail -1; echo exit=$?)"
sudo umount /mnt/dA /mnt/dB
sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"
