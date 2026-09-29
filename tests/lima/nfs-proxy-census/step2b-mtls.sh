#!/bin/bash
# Step 2b drill: RPC-with-TLS (RFC 9289) at flint-nfs-proxy with the
# REAL kernel client (xprtsec=mtls) and tlshd. Two hubs behind the
# proxy; the client certificate's URI SAN decides what a mount sees.
#
#   ./step2b-mtls.sh      (Linux >= 6.5, CONFIG_TLS, ktls-utils installed)
#
# Each leg mounts a DIFFERENT loopback address (127.0.0.x), and the
# previous identity's mounts are gone first: while an NFS client to the
# proxy exists, Linux TRUNKS a new mount of the same server (same
# server_owner) onto that client's connection, so a second certificate
# on the same node is never presented (run 1 of this drill: b's
# connection came up as b, and b's mount listed ws-a over a's). One
# node, one identity — which is what §6a's per-cluster certificate is.
set -u
BIN=${BIN:-$HOME/nfs-proxy-census/flint/spdk-csi-driver/target/release}
ROOT=$HOME/nfs-proxy-2b
OUT=${OUT:-$ROOT/out}
PX=20520
MNTS="/mnt/tA /mnt/tB /mnt/tR /mnt/tP /mnt/tC"
pass=0; fail=0
ok()  { echo "PASS $*"; pass=$((pass+1)); }
bad() { echo "FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

TLSHD_CONF=/etc/tlshd.conf
cleanup() {
  for m in $MNTS; do sudo umount -f -l $m 2>/dev/null; done
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"
  pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"
  [ -n "${CAP:-}" ] && sudo kill $CAP 2>/dev/null
  [ -f $ROOT/tlshd.conf.orig ] && sudo cp $ROOT/tlshd.conf.orig $TLSHD_CONF && sudo systemctl restart tlshd
}
trap cleanup EXIT
for m in $MNTS; do sudo umount -f -l $m 2>/dev/null; sudo mkdir -p $m; done
sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"; pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"; sleep 1
sudo rm -rf $ROOT; mkdir -p $ROOT/pki $OUT
sudo cp $TLSHD_CONF $ROOT/tlshd.conf.orig

# ---- PKI: a CA, the proxy's server cert, clients a and b, a rogue CA ----
P=$ROOT/pki
ossl() { openssl "$@" 2>/dev/null; }
mkca() {  # $1 name
  ossl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \
    -subj "/CN=$1" -keyout $P/$1.key -out $P/$1.crt
}
leaf() {  # $1 name, $2 ca, $3 SAN
  ossl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" \
    -keyout $P/$1.key -out $P/$1.csr
  printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$3" > $P/$1.ext
  ossl x509 -req -in $P/$1.csr -CA $P/$2.crt -CAkey $P/$2.key -CAcreateserial -days 2 \
    -extfile $P/$1.ext -out $P/$1.crt
}
mkca ca; mkca rogue
SAN_SRV="IP:127.0.0.1,IP:127.0.0.2,IP:127.0.0.3,IP:127.0.0.4,IP:127.0.0.5,DNS:localhost"
leaf server ca "$SAN_SRV"
leaf client-a ca "URI:spiffe://clusters/a"
leaf client-b ca "URI:spiffe://clusters/b"
leaf client-r rogue "URI:spiffe://clusters/a"
chmod 644 $P/*
check "PKI generated" '[ -s $P/server.crt ] && [ -s $P/client-a.crt ] && [ -s $P/client-r.crt ]'

tlshd_as() {  # $1 client cert name
  sudo tee $TLSHD_CONF >/dev/null <<Y
[debug]
loglevel=1
tls=0
nl=0
[authenticate]
[authenticate.client]
x509.truststore= $P/ca.crt
x509.certificate= $P/$1.crt
x509.private_key= $P/$1.key
[authenticate.server]
Y
  sudo systemctl restart tlshd; sleep 1
}

# ---- two hubs (H1 + H2) and the proxy with TLS ----
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
      setsid $BIN/flint-pnfs-mds --config $D/mds.yaml >$OUT/hub-$1.log 2>&1 < /dev/null &
}
hub ws-a 20521 10; hub ws-b 20522 11; sleep 3
sid() { sudo sed 's/\x1b\[[0-9;]*m//g' $OUT/hub-$1.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$'; }
A=$(sid ws-a); B=$(sid ws-b)
[ -n "$A" ] && [ -n "$B" ] || { echo "HUBS DID NOT START"; exit 1; }
mkdir -p $ROOT/proxy
cp $P/server.crt $ROOT/proxy/tls.crt; cp $P/server.key $ROOT/proxy/tls.key; cp $P/ca.crt $ROOT/proxy/ca.crt
cat > $ROOT/proxy/config.yaml <<Y
listen: 0.0.0.0:$PX
stateDir: $ROOT/proxy
hubs:
  - { name: ws-a, address: "127.0.0.1:20521", serverId: $A, stateidTag: 10 }
  - { name: ws-b, address: "127.0.0.1:20522", serverId: $B, stateidTag: 11 }
identities:
  - { name: cluster-a, clients: ["spiffe://clusters/a"], workspaces: ["ws-a"] }
  - { name: cluster-b, clients: ["spiffe://clusters/b"], workspaces: ["ws-b"] }
tls:
  cert: $ROOT/proxy/tls.crt
  key: $ROOT/proxy/tls.key
  clientCa: $ROOT/proxy/ca.crt
  reloadSecs: 2
Y
RUST_LOG=info setsid $BIN/flint-nfs-proxy --config $ROOT/proxy/config.yaml >$OUT/proxy.log 2>&1 < /dev/null &
sleep 1
sudo tcpdump -i lo -s 0 -U -w $OUT/2b.pcap "port $PX or port 20521" >/dev/null 2>&1 & CAP=$!; sleep 1

TLS=nfsvers=4.2,proto=tcp,port=$PX,xprtsec=mtls,soft,timeo=50,retrans=2
mnt() {  # $1 addr, $2 dir, $3 opts → rc; stderr in $OUT/mount-$(basename $2).err
  sudo timeout 60 mount -t nfs4 -o $3 $1:/ $2 2>$OUT/mount-$(basename $2).err
}
ls1() { sudo timeout 20 ls $1 2>&1 | tr '\n' ' '; }
# Unmount, then wait until the kernel has dropped every NFS client to
# the proxy (/proc/fs/nfsfs/servers lists one per server address).
drop() {
  sudo umount $1
  for i in $(seq 60); do
    grep -q " $PX " /proc/fs/nfsfs/servers 2>/dev/null || return 0
    sleep 1
  done
  echo "  (an NFS client to the proxy outlived the unmount)"; return 1
}

# ---- leg 1: client a over mTLS ----
tlshd_as client-a
mnt 127.0.0.1 /mnt/tA $TLS; rc=$?
check "client a mounts with xprtsec=mtls" '[ $rc = 0 ]'
la=$(ls1 /mnt/tA); echo "  a sees: $la"
check "client a's root lists exactly ws-a (its certificate's rule)" '[ "$la" = "ws-a " ]'
MARK=flint-2b-plaintext-marker-$RANDOM$RANDOM
echo $MARK | sudo tee /mnt/tA/ws-a/f >/dev/null
check "a write through the TLS mount lands in hub A" '[ "$(sudo cat $ROOT/ws-a/data/exports/f 2>/dev/null)" = "$MARK" ]'
check "ws-b is ENOENT to client a" '! sudo timeout 20 ls /mnt/tA/ws-b >/dev/null 2>&1'
drop /mnt/tA; rc=$?
check "client a's NFS client is gone before the next identity" '[ $rc = 0 ]'

# ---- leg 2: the SAME proxy, client b's certificate ----
tlshd_as client-b
mnt 127.0.0.2 /mnt/tB $TLS; rc=$?
check "client b mounts with xprtsec=mtls" '[ $rc = 0 ]'
lb=$(ls1 /mnt/tB); echo "  b sees: $lb"
check "client b's root lists exactly ws-b (the identity follows the certificate)" '[ "$lb" = "ws-b " ]'
echo $MARK-b | sudo tee /mnt/tB/ws-b/g >/dev/null
check "a write as client b lands in hub B" '[ "$(sudo cat $ROOT/ws-b/data/exports/g 2>/dev/null)" = "$MARK-b" ]'
drop /mnt/tB

# ---- leg 3: a certificate from another CA ----
tlshd_as client-r
mnt 127.0.0.3 /mnt/tR $TLS; rc=$?
echo "  rogue mount rc=$rc: $(head -c 200 $OUT/mount-tR.err)"
check "a certificate from another CA cannot mount" '[ $rc != 0 ]'

# ---- leg 4: no TLS at all ----
mnt 127.0.0.4 /mnt/tP nfsvers=4.2,proto=tcp,port=$PX,soft,timeo=50,retrans=2; rc=$?
echo "  plaintext mount rc=$rc: $(head -c 200 $OUT/mount-tP.err)"
check "a mount that does not upgrade is refused" '[ $rc != 0 ]'
check "the proxy refused it AUTH_TOOWEAK (the mechanism)" 'grep -q "did not upgrade to TLS: AUTH_TOOWEAK" $OUT/proxy.log'

# ---- leg 5: hot reload. The first server certificate does not name
# 127.0.0.6 and the rotated one does, so a mount of .6 succeeds only if
# the proxy SERVES the new certificate (tlshd checks the address). ----
tlshd_as client-a
mnt 127.0.0.5 /mnt/tC $TLS; rc=$?
check "client a mounts again (the mount that must survive the rotation)" '[ $rc = 0 ]'
mnt 127.0.0.6 /mnt/tP $TLS; rc=$?
check "control: before the rotation, 127.0.0.6 is not in the served certificate" '[ $rc != 0 ]'
leaf server2 ca "$SAN_SRV,IP:127.0.0.6"
cp $P/server2.key $ROOT/proxy/tls.key.new; cp $P/server2.crt $ROOT/proxy/tls.crt.new
mv $ROOT/proxy/tls.key.new $ROOT/proxy/tls.key; mv $ROOT/proxy/tls.crt.new $ROOT/proxy/tls.crt
for i in $(seq 20); do grep -q "new certificate/CA files loaded" $OUT/proxy.log && break; sleep 1; done
check "the proxy loaded the rotated certificate without a restart" 'grep -q "new certificate/CA files loaded" $OUT/proxy.log'
mnt 127.0.0.6 /mnt/tP $TLS; rc=$?
check "after the rotation, a new handshake gets the NEW certificate" '[ $rc = 0 ] && [ "$(ls1 /mnt/tP)" = "ws-a " ]'
check "a mount made before the rotation still reads" '[ "$(sudo cat /mnt/tC/ws-a/f)" = "$MARK" ]'

# ---- the wire: the payload is encrypted on the proxy's port ----
sleep 1; sudo kill $CAP; CAP=; sleep 1
hub_hits=$(sudo cat $OUT/2b.pcap | grep -a -c "$MARK")
px_hits=$(sudo tcpdump -r $OUT/2b.pcap -w - "tcp port $PX" 2>/dev/null | grep -a -c "$MARK")
echo "  marker in capture: proxy port $px_hits, hub port (plaintext, control) $((hub_hits - px_hits))"
check "control: the capture sees the payload in the clear on the hub's port" '[ $((hub_hits - px_hits)) -gt 0 ]'
check "the payload never appears in the clear on the proxy's port" '[ $px_hits = 0 ]'

echo "proxy TLS log:"; grep -E "TLS|tls:" $OUT/proxy.log | sed 's/\x1b\[[0-9;]*m//g' | cut -c1-200 | sort | uniq -c | head -20
echo "RESULT: $pass passed, $fail failed"
