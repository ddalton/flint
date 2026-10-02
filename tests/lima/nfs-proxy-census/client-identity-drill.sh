#!/bin/bash
# flint-nfs-client-identity drill (design §6a, the client DaemonSet).
#
# Part A, the HOST (real kernel, real tlshd): the agent binary, run as the
# DaemonSet would on a node, against two hubs behind flint-nfs-proxy
# (xprtsec=mtls). It must: install the Secret's files and point
# tlshd.conf at them (restarting tlshd once); carry a RENEWAL to the next
# mount without restarting tlshd; refuse a Secret whose key does not
# match, leaving the host's files alone; say "not ready" while tlshd is
# down. Each identity mounts a fresh loopback address after the previous
# client is gone (Linux trunks a second mount onto the first client).
#
# Part B, kind: the flint-nfs-client chart's DaemonSet on every node. kind
# nodes have no tlshd, so the pods must be NotReady and SAY why, with
# the files installed on each node and the node labelled false; a Secret
# update must reach the node's files.
#
#   bash client-identity-drill.sh [A|B]...     (default: A B)
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
BIN=${BIN:-$REPO/spdk-csi-driver/target/release}
MUSL=$REPO/spdk-csi-driver/target/x86_64-unknown-linux-musl/release
ROOT=$HOME/nfs-client-identity; OUT=$ROOT/out; P=$ROOT/pki
PX=20530; HOSTDIR=/etc/flint/nfs-tls; CONF=/etc/tlshd.conf
export PATH=$HOME/bin:$PATH TMPDIR=$HOME/tmp
PARTS=${*:-A B}
pass=0; fail=0
ok()  { echo "PASS $*"; pass=$((pass+1)); }
bad() { echo "FAIL $*"; fail=$((fail+1)); }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }
MNTS="/mnt/cA /mnt/cB /mnt/cC"
cleanup() {
  for m in $MNTS; do unmount_hard $m; done
  sudo pkill -TERM -f "[f]lint-pnfs-mds --config $ROOT"; pkill -TERM -f "[f]lint-nfs-proxy --config $ROOT"
  [ -f $ROOT/tlshd.conf.orig ] && sudo cp $ROOT/tlshd.conf.orig $CONF && sudo systemctl restart tlshd
  sudo rm -rf $HOSTDIR
  [ "${KEEP:-0}" = 1 ] || kind delete cluster --name flint-client-id >/dev/null 2>&1
}
trap cleanup EXIT INT TERM
for m in $MNTS; do unmount_hard $m; sudo mkdir -p $m; done
sudo rm -rf $ROOT; mkdir -p $OUT $P $TMPDIR
sudo cp $CONF $ROOT/tlshd.conf.orig; sudo chown $USER $ROOT/tlshd.conf.orig

ossl() { openssl "$@" 2>/dev/null; }
leaf() {  # name ca san
  ossl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" -keyout $P/$1.key -out $P/$1.csr
  printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$3" > $P/$1.ext
  ossl x509 -req -in $P/$1.csr -CA $P/$2.crt -CAkey $P/$2.key -CAcreateserial -days 2 -extfile $P/$1.ext -out $P/$1.crt
}
ossl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=ca" -keyout $P/ca.key -out $P/ca.crt
leaf server ca "IP:127.0.0.1,IP:127.0.0.2,IP:127.0.0.3,DNS:localhost"
leaf client-a ca "URI:spiffe://clusters/a"; leaf client-b ca "URI:spiffe://clusters/b"
chmod 644 $P/*

# A Secret volume the way kubelet projects one: files are links into a
# timestamped dir behind `..data`, swapped with one rename.
secret() {  # $1 cert name, $2 key name
  local S=$ROOT/secret; mkdir -p $S; local d=$S/..$(date +%s%N)
  mkdir $d; cp $P/$1.crt $d/tls.crt; cp $P/$2.key $d/tls.key; cp $P/ca.crt $d/ca.crt
  ln -sfn $(basename $d) $S/..data_tmp; mv -T $S/..data_tmp $S/..data
  for f in tls.crt tls.key ca.crt; do [ -L $S/$f ] || ln -s ..data/$f $S/$f; done
}
agent() {  # one pass as root, as the DaemonSet runs; prints the status
  sudo $BIN/flint-nfs-client-identity --once --status-file $ROOT/status \
    --secret-dir $ROOT/secret --install-dir $HOSTDIR --host-dir $HOSTDIR \
    --tlshd-conf $CONF --restart-cmd "systemctl restart tlshd" 2>>$OUT/agent.log
}
tlshd_since() { systemctl show tlshd -p ActiveEnterTimestampMonotonic --value; }

if [[ " $PARTS " == *" A "* ]]; then
echo "== A: the agent on a real host"
hub() {  # name port tag
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
  sudo env RUST_LOG=info FLINT_NFS_ENFORCE_PERMISSIONS=1 FLINT_FH_KERNEL=1 FLINT_NFS_FSID_FROM_VOLUME=1 FLINT_NFS_STATEID_TAG=$3 \
    setsid $BIN/flint-pnfs-mds --config $D/mds.yaml >$OUT/hub-$1.log 2>&1 < /dev/null &
}
hub ws-a 20531 10; hub ws-b 20532 11; sleep 3
sid() { sudo sed 's/\x1b\[[0-9;]*m//g' $OUT/hub-$1.log | grep -o 'server id (persistent): [0-9]*' | head -1 | grep -o '[0-9]*$'; }
A=$(sid ws-a); B=$(sid ws-b); [ -n "$A" ] && [ -n "$B" ] || { echo "HUBS DID NOT START"; exit 1; }
mkdir -p $ROOT/proxy
cat > $ROOT/proxy/config.yaml <<Y
listen: 0.0.0.0:$PX
stateDir: $ROOT/proxy
hubs:
  - { name: ws-a, address: "127.0.0.1:20531", serverId: $A, stateidTag: 10 }
  - { name: ws-b, address: "127.0.0.1:20532", serverId: $B, stateidTag: 11 }
identities:
  - { name: a, clients: ["spiffe://clusters/a"], workspaces: ["ws-a"] }
  - { name: b, clients: ["spiffe://clusters/b"], workspaces: ["ws-b"] }
tls: { cert: $P/server.crt, key: $P/server.key, clientCa: $P/ca.crt }
Y
RUST_LOG=info setsid $BIN/flint-nfs-proxy --config $ROOT/proxy/config.yaml >$OUT/proxy.log 2>&1 < /dev/null &
sleep 1
TLS=nfsvers=4.2,proto=tcp,port=$PX,xprtsec=mtls,soft,timeo=50,retrans=2
mnt() { sudo timeout 60 mount -t nfs4 -o $TLS $1:/ $2 2>$OUT/mount-$(basename $2).err; }
ls1() { sudo timeout 20 ls $1 2>&1 | tr '\n' ' '; }
drop() { sudo umount $1; for _ in $(seq 60); do grep -q " $PX " /proc/fs/nfsfs/servers 2>/dev/null || return 0; sleep 1; done; return 1; }

# Control: the node before the agent — tlshd has no client certificate.
mnt 127.0.0.1 /mnt/cA; rc=$?
check "control: before the agent, an mTLS mount fails (tlshd has no certificate)" '[ $rc != 0 ]'
drop /mnt/cA 2>/dev/null

secret client-a client-a
t0=$(tlshd_since); S=$(agent); rc=$?; echo "$S" | sed 's/^/  /'
check "first pass: ready, identity a" '[ $rc = 0 ] && echo "$S" | grep -q "identity spiffe://clusters/a"'
check "the files are on the host, the key 0600" '[ "$(sudo stat -c %a $HOSTDIR/client.key)" = 600 ] && sudo cmp -s $HOSTDIR/client.crt $P/client-a.crt'
check "tlshd.conf points at them" 'grep -q "^x509.certificate= $HOSTDIR/client.crt" $CONF'
check "tlshd was restarted for the conf edit" '[ "$(tlshd_since)" != "$t0" ]'
mnt 127.0.0.1 /mnt/cA; rc=$?
LA=$(ls1 /mnt/cA); echo "  mount as a: rc=$rc root [$LA]"
check "a mount now works and is cluster a" '[ $rc = 0 ] && [ "$LA" = "ws-a " ]'
drop /mnt/cA

secret client-b client-b
t1=$(tlshd_since); S=$(agent); rc=$?
check "renewal (the Secret now holds b): installed, still ready" '[ $rc = 0 ] && sudo cmp -s $HOSTDIR/client.crt $P/client-b.crt'
check "a renewal does not restart tlshd" '[ "$(tlshd_since)" = "$t1" ]'
mnt 127.0.0.2 /mnt/cB; rc=$?
LB=$(ls1 /mnt/cB); echo "  mount after the renewal: rc=$rc root [$LB]"
check "the next handshake presents the renewed certificate" '[ $rc = 0 ] && [ "$LB" = "ws-b " ]'

secret client-a client-b   # a half-rotated Secret: a's cert, b's key
S=$(agent); rc=$?; echo "$S" | grep reason | sed 's/^/  /'
check "a mismatched pair: not ready, and says why" '[ $rc != 0 ] && echo "$S" | grep -q "does not belong"'
check "the host keeps the last good pair" 'sudo cmp -s $HOSTDIR/client.crt $P/client-b.crt && sudo cmp -s $HOSTDIR/client.key $P/client-b.key'
check "--check agrees (the readiness probe)" '! sudo $BIN/flint-nfs-client-identity --check --status-file $ROOT/status >/dev/null'
check "the existing mount is untouched" '[ "$(ls1 /mnt/cB)" = "ws-b " ]'

secret client-b client-b
sudo systemctl stop tlshd
S=$(agent); rc=$?
check "tlshd stopped: not ready, and says so" '[ $rc != 0 ] && echo "$S" | grep -q "tlshd is not running"'
sudo systemctl start tlshd; sleep 1
S=$(agent); rc=$?
check "tlshd back: ready again" '[ $rc = 0 ]'
drop /mnt/cB
fi

if [[ " $PARTS " == *" B "* ]]; then
echo "== B: the chart's DaemonSet on kind (nodes without tlshd)"
C=flint-client-id; IMG=flint-lite-operator:client-id
D=$(mktemp -d -p $HOME); cp $MUSL/flint-nfs-client-identity $D/ || exit 1
printf 'FROM alpine:3.20\nCOPY flint-nfs-client-identity /usr/local/bin/flint-nfs-client-identity\n' > $D/Dockerfile
docker build -q -t $IMG $D >/dev/null || { echo "image build failed"; exit 1; }; rm -rf $D
kind delete cluster --name $C >/dev/null 2>&1
printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n  - role: worker\n' > $OUT/kind.yaml
kind create cluster --name $C --config $OUT/kind.yaml --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
kubectl config use-context kind-$C >/dev/null
kind load docker-image $IMG --name $C >/dev/null
kubectl create ns flint-client >/dev/null
kubectl -n flint-client create secret generic flint-nfs-client-tls --from-file=tls.crt=$P/client-a.crt --from-file=tls.key=$P/client-a.key --from-file=ca.crt=$P/ca.crt >/dev/null
helm install fnc $REPO/flint-nfs-client-chart -n flint-client --set image.ref=$IMG --set image.pullPolicy=Never \
  --set configureTlshd=false --set nodeLabel=true --set intervalSecs=5 > $OUT/helm.log 2>&1 || { cat $OUT/helm.log; exit 1; }
NODES=$(kubectl get nodes -o name | wc -l)
for _ in $(seq 60); do
  n=$(kubectl -n flint-client get pods -l app.kubernetes.io/name=flint-nfs-client --field-selector=status.phase=Running -o name | wc -l)
  [ "$n" = "$NODES" ] && break; sleep 3
done
sleep 15
nsum() { docker exec $1 sha256sum $HOSTDIR/client.crt 2>/dev/null | cut -d" " -f1; }
lsum() { sha256sum $1 | cut -d" " -f1; }
check "a pod on every node (control-plane taint tolerated)" '[ "$n" = "$NODES" ]'
READY=$(kubectl -n flint-client get pods -l app.kubernetes.io/name=flint-nfs-client -o jsonpath='{range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{" "}{end}')
echo "  pods ready: [$READY]"
check "no tlshd on kind nodes: every pod NotReady" '! echo "$READY" | grep -q True'
POD=$(kubectl -n flint-client get pods -l app.kubernetes.io/name=flint-nfs-client -o jsonpath='{.items[0].metadata.name}')
WHY=$(kubectl -n flint-client exec $POD -- /usr/local/bin/flint-nfs-client-identity --check 2>&1)
echo "$WHY" | sed 's/^/  /'
check "and it says why (tlshd), with the identity it holds" 'echo "$WHY" | grep -q "tlshd is not running" && echo "$WHY" | grep -q "identity spiffe://clusters/a"'
for node in $(kind get nodes --name $C); do
  check "the files are on $node" '[ "$(nsum $node)" = "$(lsum $P/client-a.crt)" ] && [ "$(docker exec $node stat -c %a $HOSTDIR/client.key)" = 600 ]'
done
LABELS=$(kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.labels.chert\.us/nfs-tls-ready}{" "}{end}')
check "every node labelled chert.us/nfs-tls-ready=false" '[ "$(echo $LABELS | tr " " "\n" | sort -u)" = false ]'
kubectl -n flint-client create secret generic flint-nfs-client-tls --from-file=tls.crt=$P/client-b.crt --from-file=tls.key=$P/client-b.key --from-file=ca.crt=$P/ca.crt --dry-run=client -o yaml | kubectl apply -f - >/dev/null
node=$(kind get nodes --name $C | head -1)
for _ in $(seq 60); do [ "$(nsum $node)" = "$(lsum $P/client-b.crt)" ] && break; sleep 5; done
check "a Secret update reaches the node's files (kubelet's swap, then the agent)" '[ "$(nsum $node)" = "$(lsum $P/client-b.crt)" ]'
kubectl -n flint-client logs $POD > $OUT/agent-pod.log 2>&1
fi

echo "RESULT: $pass passed, $fail failed"
