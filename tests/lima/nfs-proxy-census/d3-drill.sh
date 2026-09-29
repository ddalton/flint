#!/bin/bash
# D3 drill: copying a file from one flint mount into another must not
# fail on the ACL (census Part 4). Two hubs, each mounted DIRECTLY, plus
# the same two through flint-nfs-proxy.
#
#   BIN=<dir with flint-pnfs-mds + flint-nfs-proxy> ./d3-drill.sh
#
# Known-bad arm: BIN=<a pre-fix build> must FAIL the D3 checks and pass
# the controls.
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release}
ROOT=$HOME/nfs-proxy-d3
MNTS="/mnt/d3A /mnt/d3B /mnt/d3P"
pass=0; fail=0
ok()  { echo "PASS $*"; pass=$((pass+1)); }
bad() { echo "FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

cleanup() {
  for m in $MNTS; do sudo umount -f -l $m 2>/dev/null; done
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"
  sudo pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"
}
trap cleanup EXIT
cleanup; sleep 1
sudo rm -rf $ROOT; mkdir -p $ROOT/out
for m in $MNTS; do sudo mkdir -p $m; done

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
hub ws-a 20511 10; hub ws-b 20512 11; sleep 3

# serverId = the hub's persistent instance id, from its startup line.
sid() { sudo sed 's/\x1b\[[0-9;]*m//g' $ROOT/out/hub-$1.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$'; }
A=$(sid ws-a); B=$(sid ws-b)
proxied=1; { [ -n "$A" ] && [ -n "$B" ]; } || proxied=0
cat > $ROOT/proxy.yaml <<Y
listen: 127.0.0.1:20510
stateDir: $ROOT/proxy
leaseSecs: 90
hubs:
  - { name: ws-a, address: "127.0.0.1:20511", serverId: ${A:-0}, stateidTag: 10 }
  - { name: ws-b, address: "127.0.0.1:20512", serverId: ${B:-0}, stateidTag: 11 }
identities:
  - { name: all, sources: ["0.0.0.0/0"], workspaces: ["*"] }
Y
if [ $proxied = 1 ]; then
  mkdir -p $ROOT/proxy
  RUST_LOG=info setsid $BIN/flint-nfs-proxy --config $ROOT/proxy.yaml >$ROOT/out/proxy.log 2>&1 < /dev/null &
  sleep 2
fi

O=nfsvers=4.2,proto=tcp,hard,timeo=50
sudo mount -t nfs4 -o $O,port=20511 127.0.0.1:/ /mnt/d3A
sudo mount -t nfs4 -o $O,port=20512 127.0.0.1:/ /mnt/d3B
[ $proxied = 1 ] && sudo mount -t nfs4 -o $O,port=20510 127.0.0.1:/ /mnt/d3P

# --- controls: the mode path still works, and the copy moves bytes ---
echo hello-a | sudo tee /mnt/d3A/f >/dev/null
sudo chmod 640 /mnt/d3A/f
check "control: chmod lands (mode 640)" '[ "$(stat -c %a /mnt/d3A/f)" = 640 ]'

# --- D3, direct: cp -a and mv from one flint mount into another ---
out=$(sudo timeout 20 cp -a /mnt/d3A/f /mnt/d3B/cp 2>&1); rc=$?
echo "  cp -a: rc=$rc err=[$out]"
check "direct cp -a exits 0 with no error" '[ $rc = 0 ] && [ -z "$out" ]'
check "control: cp -a moved the bytes" '[ "$(sudo cat /mnt/d3B/cp)" = hello-a ]'
check "cp -a preserved the mode (640) without an ACL" '[ "$(stat -c %a /mnt/d3B/cp)" = 640 ]'
out=$(sudo timeout 20 mv /mnt/d3A/f /mnt/d3B/mv 2>&1); rc=$?
echo "  mv: rc=$rc err=[$out]"
check "direct mv exits 0 with no error" '[ $rc = 0 ] && [ -z "$out" ]'

# The mechanism: the client no longer offers system.nfs4_acl.
xa=$(sudo python3 -c "import os; print(os.listxattr('/mnt/d3B/mv'))" 2>&1)
echo "  listxattr: $xa"
check "system.nfs4_acl is not listed" '! echo "$xa" | grep -q nfs4_acl'

# --- D3 through the proxy: the proxy answers GETATTR from the same
# encoder for its root, and passes each hub's answer through ---
if [ $proxied = 1 ]; then
  echo hello-p | sudo tee /mnt/d3P/ws-a/p >/dev/null
  sudo chmod 640 /mnt/d3P/ws-a/p
  out=$(sudo timeout 20 cp -a /mnt/d3P/ws-a/p /mnt/d3P/ws-b/p 2>&1); rc=$?
  echo "  proxied cp -a: rc=$rc err=[$out]"
  check "proxied cp -a exits 0 with no error" '[ $rc = 0 ] && [ -z "$out" ]'
  check "control: proxied cp -a moved the bytes" '[ "$(sudo cat /mnt/d3P/ws-b/p)" = hello-p ]'
  out=$(sudo timeout 20 mv /mnt/d3P/ws-a/p /mnt/d3P/ws-b/q 2>&1); rc=$?
  echo "  proxied mv: rc=$rc err=[$out]"
  check "proxied mv exits 0 with no error" '[ $rc = 0 ] && [ -z "$out" ]'
else
  echo "INCONCLUSIVE proxied leg: no instance id in the hub logs"
fi

echo "RESULT: $pass passed, $fail failed"
