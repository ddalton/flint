#!/bin/bash
# nfs-proxy step 2, end to end on a real kernel: two real flint lite hubs
# (H1 + H2 on) behind flint-nfs-proxy, one Linux NFSv4.2 mount of the
# proxy's `/`. Judged where the consumer sees it (the mount) AND where
# the bytes must land (each hub's export dir on the host).
#
#   bash step2-e2e.sh            # the proxied run
#   ALLOW='"ws-*"' bash step2-e2e.sh   # allowlist CONTROL: the two allowlist checks must FAIL
# Evidence: $ROOT/out/{proxy.log,hub-*.log,e2e.pcap}.
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release}
ROOT=/tmp/nfs-proxy-step2; MNT=/mnt/px; PX=20490
sudo umount -f $MNT 2>/dev/null
sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT" ; pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"; sleep 1
sudo rm -rf $ROOT; mkdir -p $ROOT/out; sudo mkdir -p $MNT

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
sid() { sed 's/\x1b\[[0-9;]*m//g' $ROOT/out/hub-$1.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$'; }

hub ws-a 20491 10
hub ws-b 20492 11
sleep 3
A=$(sid ws-a); B=$(sid ws-b)
echo "hub ws-a server id $A, ws-b $B"
[ -n "$A" ] && [ -n "$B" ] || { echo "HUBS DID NOT START"; tail -5 $ROOT/out/hub-*.log; exit 1; }

mkdir -p $ROOT/proxy
cat > $ROOT/proxy/config.yaml <<Y
listen: 127.0.0.1:$PX
stateDir: $ROOT/proxy
hubs:
  - { name: ws-a, address: "127.0.0.1:20491", serverId: $A, stateidTag: 10 }
  - { name: ws-b, address: "127.0.0.1:20492", serverId: $B, stateidTag: 11 }
  - { name: ws-hidden, address: "127.0.0.1:20493", serverId: 1, stateidTag: 12 }
identities:
  - { name: local, sources: ["127.0.0.1/32"], workspaces: [${ALLOW:-"ws-a", "ws-b"}] }
Y
RUST_LOG=${PROXY_LOG:-info} setsid $BIN/flint-nfs-proxy --config $ROOT/proxy/config.yaml >$ROOT/out/proxy.log 2>&1 < /dev/null &
sleep 1
sudo tcpdump -i lo -s 0 -w $ROOT/out/e2e.pcap port $PX or port 20491 or port 20492 >/dev/null 2>&1 & CAP=$!; sleep 1

ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }

sudo timeout 30 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$PX 127.0.0.1:/ $MNT
check "mount" "mountpoint -q $MNT"
LS=$(timeout 10 ls $MNT 2>&1 | tr '\n' ' ')
echo "ls /: $LS"
check "root lists exactly the allowed workspaces" '[ "$LS" = "ws-a ws-b " ]'
echo hello-a | sudo timeout 10 tee $MNT/ws-a/f >/dev/null
echo hello-b | sudo timeout 10 tee $MNT/ws-b/f >/dev/null
check "ws-a write landed in hub A's export" '[ "$(sudo cat $ROOT/ws-a/data/exports/f 2>/dev/null)" = hello-a ]'
check "ws-b write landed in hub B's export" '[ "$(sudo cat $ROOT/ws-b/data/exports/f 2>/dev/null)" = hello-b ]'
check "read back through the proxy" '[ "$(timeout 10 cat $MNT/ws-a/f)" = hello-a ] && [ "$(timeout 10 cat $MNT/ws-b/f)" = hello-b ]'
DA=$(stat -c %d $MNT/ws-a/f); DB=$(stat -c %d $MNT/ws-b/f); DR=$(stat -c %d $MNT)
echo "st_dev: / $DR, ws-a $DA, ws-b $DB"
check "each workspace is its own filesystem to the client (H1)" '[ "$DA" != "$DB" ] && [ "$DA" != "$DR" ]'
sudo mkdir -p $MNT/ws-a/d && sudo timeout 10 sh -c "for i in \$(seq 1 200); do echo \$i > $MNT/ws-a/d/n\$i; done"
check "200 creates in ws-a" '[ "$(ls $MNT/ws-a/d | wc -l)" = 200 ] && [ "$(sudo ls $ROOT/ws-a/data/exports/d | wc -l)" = 200 ]'
ERR=$(sudo timeout 10 mv $MNT/ws-a/f $MNT/ws-b/g 2>&1); echo "cross rename: $ERR"
# EXDEV makes mv copy + unlink. The copy's "preserving permissions: EIO"
# is hub defect D3 (SETATTR of the empty ACL the hub itself reports is
# ATTRNOTSUPP) — the control arm, step2-control-direct.sh, shows it
# between two DIRECT mounts too. The data must still have moved.
check "a cross-workspace mv moves the data (EXDEV → copy + unlink)" '[ "$(sudo cat $ROOT/ws-b/data/exports/g 2>/dev/null)" = hello-a ] && ! sudo test -e $ROOT/ws-a/data/exports/f'
# ENOENT, not merely a failure: ws-hidden's hub does not exist, so an
# ALLOWED lookup would fail too (DELAY until the timeout) and pass a
# looser check.
HID=$(timeout 10 ls $MNT/ws-hidden 2>&1); echo "ls ws-hidden: $HID"
check "the hidden workspace is ENOENT, like an absent one" 'echo "$HID" | grep -q "No such file or directory"'
python3 - <<P
import fcntl, os
fd = os.open("$MNT/ws-b/lk", os.O_CREAT | os.O_RDWR)
fcntl.lockf(fd, fcntl.LOCK_EX); fcntl.lockf(fd, fcntl.LOCK_UN); os.close(fd)
print("lock/unlock ok")
P
check "a byte-range lock through the proxy" '[ $? = 0 ]'
dd if=/dev/urandom of=$ROOT/out/big bs=1M count=64 2>/dev/null
sudo timeout 60 cp $ROOT/out/big $MNT/ws-b/big
check "64 MiB round trip is byte-identical" '[ "$(md5sum < $ROOT/out/big)" = "$(sudo md5sum < $ROOT/ws-b/data/exports/big)" ] && [ "$(md5sum < $ROOT/out/big)" = "$(timeout 60 md5sum < $MNT/ws-b/big)" ]'

sudo umount $MNT; check "umount" '! mountpoint -q $MNT'
sleep 1; sudo kill $CAP; wait $CAP 2>/dev/null
T() { sudo cat $ROOT/out/e2e.pcap | tshark -r - -d tcp.port==$PX,rpc -d tcp.port==20491,rpc -d tcp.port==20492,rpc "$@" 2>/dev/null; }
XDEV=$(T -Y "tcp.srcport==$PX && nfs.opcode==60 && nfs.nfsstat4==18" | wc -l)
HUBCOPY=$(T -Y "(tcp.dstport==20491 || tcp.dstport==20492) && nfs.opcode==60" | wc -l)
echo "COPY→XDEV replies from the proxy: $XDEV; COPY calls reaching a hub: $HUBCOPY"
check "the proxy answered the cross-workspace COPY itself (XDEV), no hub saw it" '[ "$XDEV" -ge 1 ] && [ "$HUBCOPY" = 0 ]'
SECOND=$(grep -c "second target" $ROOT/out/proxy.log)
check "second-target refusals = 0 (design §3)" '[ "$SECOND" = 0 ]'
echo "proxy warnings/errors:"; sed 's/\x1b\[[0-9;]*m//g' $ROOT/out/proxy.log | grep -E "WARN|ERROR" | cut -c1-220 | sort | uniq -c | head -20
pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"; sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"
sudo chmod -R a+r $ROOT/out
echo "RESULT: $ok passed, $bad failed"
