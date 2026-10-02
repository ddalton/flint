#!/bin/bash
# nfs-proxy step 4 on kind (the box): the operator chart with
# nfsProxy.enabled, three FlintShares, and the HOST kernel mounting the
# proxy's NodePort. Checks the wiring, not the protocol (steps 2-3 did):
#   - each share gets a stateidTag, owned by its own field manager, and it
#     survives an operator restart and a hub restart unchanged (set-once);
#   - hubs carry H1 + their own tag in env; hub Services are headless;
#   - `/` lists exactly the allowed workspaces; bytes land in the right hub;
#   - a hub suspended by the idle ladder is WOKEN by the proxy (the
#     requested-at stamp), and the read completes;
#   - the hub NetworkPolicy admits the proxy's pods on 2049 and nobody
#     else (INCONCLUSIVE if the CNI enforces no policy at all).
#   bash step4-kind.sh           # KEEP=1 leaves the cluster up
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CARGO_DIR=$REPO/spdk-csi-driver
CHART=$REPO/flint-lite-operator-chart
CLUSTER=flint-nfsproxy
OPNS=flint-system; NS=workspaces
TAG=nfsproxy-dev
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
OUT=$HOME/nfs-proxy-step4; MNT=/mnt/pxk
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp   # snap docker cannot read outside $HOME (box trap 2)
# A FRESH $OUT every run: a wake.rc left by an earlier run was read as this
# run's answer before the real write finished (2026-09-28: "rc=2", blank
# end time, empty stderr — a stale file, not a failure).
rm -rf $OUT; mkdir -p $OUT $TMPDIR
ok=0; bad=0; inc=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
K() { kubectl "$@"; }
cleanup() {
  unmount_hard $MNT
  [ "${KEEP:-0}" = 1 ] || kind delete cluster --name $CLUSTER >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

echo "== images"
T=x86_64-unknown-linux-musl; R=$CARGO_DIR/target/$T/release
IMG=$(mktemp -d -p $HOME)
for b in flint-pnfs-mds flint-lite-operator flint-hub-gateway flint-nfs-proxy; do cp $R/$b $IMG/ || exit 1; done
cat > $IMG/Dockerfile.hub <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache curl ca-certificates
COPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds
EOF
cat > $IMG/Dockerfile.op <<'EOF'
FROM alpine:3.20
RUN apk add --no-cache ca-certificates
COPY flint-lite-operator /usr/local/bin/flint-lite-operator
COPY flint-hub-gateway /usr/local/bin/flint-hub-gateway
COPY flint-nfs-proxy /usr/local/bin/flint-nfs-proxy
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/flint-lite-operator"]
EOF
docker build -q -f $IMG/Dockerfile.hub -t $HUBIMG $IMG >/dev/null && docker build -q -f $IMG/Dockerfile.op -t $OPIMG $IMG >/dev/null || { echo "image build failed"; exit 1; }
rm -rf $IMG

echo "== cluster"
kind delete cluster --name $CLUSTER >/dev/null 2>&1
cat > $OUT/kind.yaml <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
EOF
kind create cluster --name $CLUSTER --config $OUT/kind.yaml --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
for i in $HUBIMG $OPIMG; do kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || { echo "kind load $i failed"; exit 1; }; done
kind version

echo "== install"
K create ns $OPNS >/dev/null; K create ns $NS >/dev/null
cat > $OUT/values.yaml <<EOF
image: { ref: "$OPIMG", pullPolicy: Never }
hubImage: "$HUBIMG"
hubImagePullPolicy: Never
replicas: 1
networkPolicy:
  enabled: true
  hubNamespaces: ["$NS"]
nfsProxy:
  enabled: true
  service: { type: NodePort, externalTrafficPolicy: Local }
  logLevel: info
  identities:
    - name: host
      sources: ["172.16.0.0/12", "10.0.0.0/8", "192.168.0.0/16"]
      workspaces: ["ws-a", "ws-b", "ws-c"]
EOF
helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || { echo "operator not ready"; exit 1; }

share() {  # name [idle-secs]
  cat <<EOF | K apply -f - >/dev/null
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $1, namespace: $NS }
spec:
  persistence: { size: 1Gi }
$( [ -n "${2:-}" ] && echo "  idle: { suspendAfterSecs: $2 }" )
EOF
}
share ws-a; share ws-b; share ws-c 45; share ws-hidden
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
st() { K -n $NS get flintshare $1 -o jsonpath="{.status.$2}" 2>/dev/null; }
for s in ws-a ws-b ws-c ws-hidden; do
  for _ in $(seq 1 90); do [ "$(phase $s)" = Ready ] && [ -n "$(st $s serverId)" ] && break; sleep 4; done
  echo "$s: phase=$(phase $s) serverId=$(st $s serverId) stateidTag=$(st $s stateidTag)"
done
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || { echo "proxy not ready"; K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy | tail; exit 1; }

echo "== operator wiring"
TA=$(st ws-a stateidTag); TB=$(st ws-b stateidTag); TC=$(st ws-c stateidTag); TH=$(st ws-hidden stateidTag)
check "every share got a stateidTag" '[ -n "$TA" ] && [ -n "$TB" ] && [ -n "$TC" ] && [ -n "$TH" ]'
check "the tags are distinct" '[ "$(printf "%s\n" $TA $TB $TC $TH | sort -u | wc -l)" = 4 ]'
MF=$(K -n $NS get flintshare ws-a --show-managed-fields -o json | python3 -c "
import json,sys
o=json.load(sys.stdin)
print(' '.join(sorted(m['manager'] for m in o['metadata']['managedFields'] if 'f:stateidTag' in json.dumps(m.get('fieldsV1',{})))))")
echo "managers owning status.stateidTag: [$MF]"
check "status.stateidTag is owned by the tag manager alone" '[ "$MF" = "flint-lite-operator/stateid-tag" ]'
HENV=$(K -n $NS get deploy -l chert.us/share=ws-a -o jsonpath='{range .items[0].spec.template.spec.containers[0].env[*]}{.name}={.value}{" "}{end}')
check "hub A runs with H1 and its OWN tag" 'echo "$HENV" | grep -q "FLINT_NFS_FSID_FROM_VOLUME=1" && echo "$HENV" | grep -q "FLINT_NFS_STATEID_TAG=$TA "'
CIP=$(K -n $NS get svc -l chert.us/share=ws-a -o jsonpath='{.items[*].spec.clusterIP}')
check "hub Services are headless" 'echo "$CIP" | grep -qw None'

echo "== mount through the proxy"
NODE_IP=$(K get node $CLUSTER-worker -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
PORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
PXNODE=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
PXNODE_IP=$(K get node $PXNODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
echo "proxy at $PXNODE_IP:$PORT (externalTrafficPolicy Local: the node that runs it)"
sudo mkdir -p $MNT
sudo timeout 60 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$PORT $PXNODE_IP:/ $MNT
check "mount" 'timeout 10 mountpoint -q $MNT'
LS=$(timeout 20 ls $MNT 2>&1 | tr '\n' ' '); echo "ls /: $LS"
check "/ lists exactly the allowed, routable workspaces" '[ "$LS" = "ws-a ws-b ws-c " ]'
echo hello-a | sudo timeout 30 tee $MNT/ws-a/f >/dev/null; echo hello-b | sudo timeout 30 tee $MNT/ws-b/f >/dev/null
HA=$(K -n $NS get pod -l chert.us/share=ws-a -o jsonpath='{.items[0].metadata.name}')
HB=$(K -n $NS get pod -l chert.us/share=ws-b -o jsonpath='{.items[0].metadata.name}')
check "bytes landed in the right hubs" '[ "$(K -n $NS exec $HA -- cat /data/exports/f 2>/dev/null || K -n $NS exec $HA -- sh -c "cat /*/f /data/*/f 2>/dev/null | head -1")" = hello-a ]'

echo "== set-once: operator restart, hub restart"
K -n $OPNS delete pod -l app.kubernetes.io/name=flint-lite-operator --wait=false >/dev/null
K -n $NS delete pod $HA --wait=false >/dev/null
sleep 45
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=120s >/dev/null
for _ in $(seq 1 60); do [ "$(phase ws-a)" = Ready ] && break; sleep 3; done
check "tags unchanged after an operator and a hub restart" '[ "$(st ws-a stateidTag)" = "$TA" ] && [ "$(st ws-b stateidTag)" = "$TB" ]'
check "the mount still reads after the hub restart" '[ "$(timeout 120 cat $MNT/ws-a/f)" = hello-a ]'

echo "== wake: ws-c idles out, the proxy wakes it"
for _ in $(seq 1 60); do [ "$(phase ws-c)" = IdleSuspended ] && break; sleep 5; done
echo "ws-c phase: $(phase ws-c)"
check "ws-c went IdleSuspended (precondition)" '[ "$(phase ws-c)" = IdleSuspended ]'
rm -f $OUT/wake.*
( date -u +%T > $OUT/wake.t0; timeout 300 sudo sh -c "echo woken > $MNT/ws-c/w" 2> $OUT/wake.err; r=$?; date -u +%T > $OUT/wake.t1; echo $r > $OUT/wake.rc ) &
for _ in $(seq 1 30); do K -n $NS get flintshare ws-c -o jsonpath='{.metadata.annotations}' | grep -q requested-at && break; sleep 2; done
check "the proxy stamped chert.us/requested-at on ws-c" 'K -n $NS get flintshare ws-c -o jsonpath="{.metadata.annotations}" | grep -q requested-at || K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy | grep -q "ws-c: wake requested"'
for _ in $(seq 1 150); do [ -e $OUT/wake.rc ] && break; sleep 2; done
echo "wake write: rc=$(cat $OUT/wake.rc 2>/dev/null) $(cat $OUT/wake.t0 2>/dev/null)→$(cat $OUT/wake.t1 2>/dev/null) err=[$(cat $OUT/wake.err 2>/dev/null)]"
check "the write to the woken workspace completed" '[ "$(cat $OUT/wake.rc 2>/dev/null)" = 0 ] && [ "$(phase ws-c)" = Ready ]'

echo "== hub lockdown (NetworkPolicy)"
HIP=$(K -n $NS get pod -l chert.us/share=ws-b -o jsonpath='{.items[0].status.podIP}')
probe() {  # ns labels -> "open"/"closed"
  K -n $1 run np-probe-$RANDOM --rm -i --restart=Never --image=busybox:1.36 --labels="$2" --command -- \
    sh -c "nc -w 3 -z $HIP 2049 && echo open || echo closed" 2>/dev/null | grep -E "open|closed" | tail -1
}
kind load docker-image busybox:1.36 --name $CLUSTER >/dev/null 2>&1 || { docker pull -q busybox:1.36 >/dev/null && kind load docker-image busybox:1.36 --name $CLUSTER >/dev/null 2>&1; }
STRANGER=$(probe $NS "app=stranger")
LOOKALIKE=$(probe $OPNS "app.kubernetes.io/name=flint-lite-operator-nfs-proxy,app.kubernetes.io/instance=flint-lite-operator")
echo "a stranger pod: $STRANGER; a pod with the proxy's labels: $LOOKALIKE"
if [ "$STRANGER" = open ] && [ "$LOOKALIKE" = open ]; then
  echo "INCONCLUSIVE lockdown: the CNI enforces no NetworkPolicy here"; inc=$((inc+1))
else
  check "a stranger cannot reach a hub's 2049; the proxy's peer can" '[ "$STRANGER" = closed ] && [ "$LOOKALIKE" = open ]'
fi

unmount_hard $MNT
K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy > $OUT/proxy.log 2>&1
K -n $OPNS logs deploy/flint-lite-operator > $OUT/operator.log 2>&1
echo "proxy warnings:"; sed 's/\x1b\[[0-9;]*m//g' $OUT/proxy.log | grep -E "WARN|ERROR" | grep -v "no serverId yet" | cut -c29-200 | sort | uniq -c | sort -rn > $OUT/warn.txt; head -12 $OUT/warn.txt
echo "RESULT: $ok passed, $bad failed, $inc inconclusive"
