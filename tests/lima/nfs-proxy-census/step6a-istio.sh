#!/bin/bash
# nfs-proxy design §6a "Checks before code", on kind + Istio AMBIENT (the
# box). The HOST kernel mounts with xprtsec=mtls (tlshd), so the client
# side is the real one (check 6). Answers the §6a questions and checks
# what the chart's istio block promises:
#
#   Q1  bypass-inbound-capture exists, and the ingress gateway then
#       reaches the proxy as plain TCP (an mTLS mount through it works)
#   Q2  path A keeps the client's source address (a rule needing the
#       certificate AND the host's address grants ws-b)
#   Q3  the hub's kubelet TCP probe reaches the hub, not ztunnel
#       (REJECT 2049 inside the hub's netns: the pod must go NotReady)
#   Q4  a ztunnel restart on the proxy's node resets client connections
#       without the bypass, and not with it (kernel connect_count)
#   Q5  tlshd presents a rotated client certificate without a restart
#   +   hub lockdown: a pod with the proxy's labels but not its
#       ServiceAccount gets no NFS reply; the proxy's ServiceAccount does
#
#   bash step6a-istio.sh      # KEEP=1 leaves the cluster up
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CARGO_DIR=$REPO/spdk-csi-driver
CHART=$REPO/flint-lite-operator-chart
CLUSTER=flint-istio
OPNS=flint-system; NS=workspaces
TAG=nfsproxy-istio
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
OUT=$HOME/nfs-proxy-istio; P=$OUT/pki
# Istio 1.31 ignores Gateway API CRDs older than v1.6 (run 1: v1.3.0, the
# TCPRoute never attached and path B looked like a bypass failure).
GWAPI=${GWAPI:-v1.6.2}
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp
rm -rf $OUT; mkdir -p $OUT $P $TMPDIR
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
answer() { echo "ANSWER $1: $2"; echo "$1: $2" >> $OUT/answers.txt; }
K() { kubectl "$@"; }
MNTS="/mnt/iA /mnt/iG /mnt/iZ /mnt/iR"
TLSHD_CONF=/etc/tlshd.conf
cleanup() {
  for m in $MNTS; do unmount_hard $m; done
  [ -f $OUT/tlshd.conf.orig ] && sudo cp $OUT/tlshd.conf.orig $TLSHD_CONF && sudo systemctl restart tlshd
  [ "${KEEP:-0}" = 1 ] || kind delete cluster --name $CLUSTER >/dev/null 2>&1
}
trap cleanup EXIT INT TERM
for m in $MNTS; do unmount_hard $m; sudo mkdir -p $m; done
sudo cp $TLSHD_CONF $OUT/tlshd.conf.orig

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

echo "== cluster + Istio ambient"
kind delete cluster --name $CLUSTER >/dev/null 2>&1
cat > $OUT/kind.yaml <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
  - role: worker
EOF
kind create cluster --name $CLUSTER --config $OUT/kind.yaml --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
K config use-context kind-$CLUSTER >/dev/null
for i in $HUBIMG $OPIMG busybox:1.36; do
  docker image inspect $i >/dev/null 2>&1 || docker pull -q $i >/dev/null
  kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || { echo "kind load $i failed"; exit 1; }
done
K apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/$GWAPI/standard-install.yaml >/dev/null || { echo "Gateway API CRDs failed"; exit 1; }
istioctl install --set profile=ambient --skip-confirmation > $OUT/istio-install.log 2>&1 || { tail $OUT/istio-install.log; exit 1; }
istioctl version 2>/dev/null | head -3
for ns in $OPNS $NS; do K create ns $ns >/dev/null; K label ns $ns istio.io/dataplane-mode=ambient >/dev/null; done

echo "== PKI (server SANs = every node address) and tlshd"
ossl() { openssl "$@" 2>/dev/null; }
leaf() {  # name ca san
  ossl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" -keyout $P/$1.key -out $P/$1.csr
  printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$3" > $P/$1.ext
  ossl x509 -req -in $P/$1.csr -CA $P/$2.crt -CAkey $P/$2.key -CAcreateserial -days 2 -extfile $P/$1.ext -out $P/$1.crt
}
ossl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=ca" -keyout $P/ca.key -out $P/ca.crt
NODE_IPS=$(K get nodes -o jsonpath='{range .items[*]}{.status.addresses[?(@.type=="InternalIP")].address}{" "}{end}')
SAN=$(for ip in $NODE_IPS; do printf 'IP:%s,' $ip; done)DNS:localhost
leaf server ca "$SAN"; leaf client-a ca "URI:spiffe://clusters/a"; leaf client-b ca "URI:spiffe://clusters/b"
chmod 644 $P/*
# tlshd reads FIXED paths; Q5 rewrites what is at them.
cp $P/client-a.crt $P/client.crt; cp $P/client-a.key $P/client.key
sudo tee $TLSHD_CONF >/dev/null <<Y
[debug]
loglevel=1
tls=0
nl=0
[authenticate]
[authenticate.client]
x509.truststore= $P/ca.crt
x509.certificate= $P/client.crt
x509.private_key= $P/client.key
[authenticate.server]
Y
sudo systemctl restart tlshd
K -n $OPNS create secret tls px-tls --cert=$P/server.crt --key=$P/server.key >/dev/null
K -n $OPNS create configmap flint-client-ca --from-file=ca.crt=$P/ca.crt >/dev/null
HOST_IP=$(docker network inspect kind -f '{{range .IPAM.Config}}{{.Gateway}} {{end}}' | tr ' ' '\n' | grep -m1 '\.')
echo "node addresses: $NODE_IPS; the host on the kind network: $HOST_IP"

echo "== Istio ingress Gateway (path B)"
K create ns istio-ingress >/dev/null
cat <<EOF | K apply -f - >/dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: nfs-gw
  namespace: istio-ingress
  annotations: { networking.istio.io/service-type: NodePort }
spec:
  gatewayClassName: istio
  listeners:
    - name: nfs
      port: 2049
      protocol: TCP
      allowedRoutes:
        namespaces: { from: All }
        kinds: [ { kind: TCPRoute } ]
EOF

echo "== install"
values() {  # $1 = bypassInboundCapture
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
    - { name: a, clients: ["spiffe://clusters/a"], workspaces: ["ws-a"] }
    - { name: a-from-host, clients: ["spiffe://clusters/a"], sources: ["$HOST_IP/32"], workspaces: ["ws-b"] }
    - { name: b, clients: ["spiffe://clusters/b"], workspaces: ["ws-c"] }
  tls:
    enabled: true
    secretName: px-tls
    clientCa: { configMapName: flint-client-ca }
  istio:
    enabled: true
    bypassInboundCapture: $1
    tcpRoute:
      enabled: true
      parentRef: { name: nfs-gw, namespace: istio-ingress, sectionName: nfs }
EOF
}
values true
helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=240s >/dev/null || { echo "operator not ready"; exit 1; }
for s in ws-a ws-b ws-c; do
  cat <<EOF | K apply -f - >/dev/null
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $s, namespace: $NS }
spec:
  persistence: { size: 1Gi }
EOF
done
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
st() { K -n $NS get flintshare $1 -o jsonpath="{.status.$2}" 2>/dev/null; }
for s in ws-a ws-b ws-c; do
  for _ in $(seq 1 90); do [ "$(phase $s)" = Ready ] && [ -n "$(st $s serverId)" ] && break; sleep 4; done
  echo "$s: phase=$(phase $s) serverId=$(st $s serverId) stateidTag=$(st $s stateidTag)"
done
check "the operator reaches every hub's status port through the mesh (serverId set)" '[ -n "$(st ws-a serverId)" ] && [ -n "$(st ws-b serverId)" ] && [ -n "$(st ws-c serverId)" ]'
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || { echo "proxy not ready"; K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy | tail; exit 1; }
PXPOD=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].metadata.name}')
check "the proxy pod carries bypass-inbound-capture" '[ "$(K -n $OPNS get pod $PXPOD -o jsonpath="{.metadata.annotations.ambient\.istio\.io/bypass-inbound-capture}")" = true ]'
check "ztunnel enrolled the proxy pod (its OUTBOUND is in the mesh)" '[ "$(K -n $OPNS get pod $PXPOD -o jsonpath="{.metadata.annotations.ambient\.istio\.io/redirection}")" = enabled ]'

TLS="nfsvers=4.2,proto=tcp,xprtsec=mtls,soft,timeo=100,retrans=3"
mnt() { sudo timeout 90 mount -t nfs4 -o $3,port=$2 $1:/ $4 2>$OUT/mount-$(basename $4).err; }
ls1() { sudo timeout 30 ls $1 2>&1 | tr '\n' ' '; }
drop() {  # umount, then wait for the kernel to drop its client to $2
  unmount_hard $1
  for _ in $(seq 60); do grep -q "$2" /proc/fs/nfsfs/servers 2>/dev/null || return 0; sleep 1; done; return 1
}
connects() {  # kernel connect_count of the transport behind mount $1
  awk -v m="$1" '$0 ~ "mounted on "m" " {f=1} f && /xprt:/ {print $5; exit}' /proc/self/mountstats
}
pxnode() {
  local n=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
  echo "$n $(K get node $n -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')"
}
read PXNODE PXIP <<<"$(pxnode)"
PXPORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
echo "path A: $PXIP:$PXPORT (proxy on $PXNODE)"

echo "== Q2 path A: mTLS mount, source address"
mnt $PXIP $PXPORT $TLS /mnt/iA; rc=$?
check "path A: an mTLS mount of the proxy's NodePort" '[ $rc = 0 ]'
LA=$(ls1 /mnt/iA); echo "  root: $LA"
check "path A: the certificate's workspace is there" 'echo "$LA" | grep -qw ws-a'
if echo "$LA" | grep -qw ws-b; then answer Q2 "yes: the proxy saw the host's address ($HOST_IP), and the cert+address rule matched"; else answer Q2 "NO: the cert+address rule did not match (source rewritten)"; fi
check "path A: b's workspace is not" '! echo "$LA" | grep -qw ws-c'
MARK=istio-$RANDOM$RANDOM
echo $MARK | sudo timeout 60 tee /mnt/iA/ws-a/f >/dev/null
check "path A: a write through the mesh lands in hub A" '[ "$(K -n $NS exec deploy/$(K -n $NS get deploy -l chert.us/share=ws-a -o jsonpath={.items[0].metadata.name}) -c hub -- cat /data/exports/f 2>/dev/null)" = "$MARK" ]'

echo "== Q4 ztunnel restart on the proxy's node, WITH the bypass"
zrestart() {
  K -n istio-system delete pod -l app=ztunnel --field-selector spec.nodeName=$PXNODE --wait=true >/dev/null
  K -n istio-system rollout status ds/ztunnel --timeout=120s >/dev/null
}
c0=$(connects /mnt/iA); zrestart
r=$(sudo timeout 120 cat /mnt/iA/ws-a/f 2>&1); echo x-$RANDOM | sudo timeout 120 tee /mnt/iA/ws-a/g >/dev/null; w=$?
c1=$(connects /mnt/iA)
echo "  connect_count $c0 → $c1; read [$r], write rc=$w"
check "with the bypass, I/O continues across a ztunnel restart (the proxy re-dials its hubs)" '[ "$r" = "$MARK" ] && [ $w = 0 ]'
BYPASS_RESET=$([ "$c1" != "$c0" ] && echo yes || echo no)

echo "== Q1 path B: through the Istio ingress Gateway"
GWPORT=$(K -n istio-ingress get svc -l gateway.networking.k8s.io/gateway-name=nfs-gw -o jsonpath='{.items[0].spec.ports[?(@.port==2049)].nodePort}')
K -n $OPNS get tcproute -o yaml > $OUT/tcproute.yaml 2>&1
ATTACHED=$(K -n istio-ingress get gateway nfs-gw -o jsonpath='{.status.listeners[0].attachedRoutes}')
echo "  gateway NodePort $GWPORT, routes attached to its listener: $ATTACHED"
check "precondition: the TCPRoute is attached to the Gateway's listener" '[ "$ATTACHED" = 1 ]'

drop /mnt/iA $PXIP || echo "  (a client to $PXIP lingered)"
GWIP=$(echo $NODE_IPS | tr ' ' '\n' | grep -v "^$PXIP$" | head -1)
mnt $GWIP $GWPORT $TLS /mnt/iG; rc=$?
echo "  mount via $GWIP:$GWPORT rc=$rc $(head -c 200 $OUT/mount-iG.err)"
LG=$(ls1 /mnt/iG); echo "  root: $LG"
if [ "$ATTACHED" != 1 ]; then
  answer Q1 "UNDECIDED: the route never attached, so the gateway had nothing to forward (not a bypass verdict)"
elif [ $rc = 0 ] && echo "$LG" | grep -qw ws-a; then
  answer Q1 "yes: the gateway carries RPC-with-TLS to the bypassed proxy as plain TCP (an mTLS mount through it works)"
else
  answer Q1 "NO: an mTLS mount through the gateway failed (see mount-iG.err, proxy.log) — use the PERMISSIVE fallback"
fi
check "path B: the certificate still names the client (ws-a)" 'echo "$LG" | grep -qw ws-a'
check "path B: the gateway's address does not satisfy the host-address rule (no ws-b)" '! echo "$LG" | grep -qw ws-b'
drop /mnt/iG $GWIP

echo "== Q5 tlshd and a rotated client certificate (no restart)"
cp $P/client-b.crt $P/client.crt; cp $P/client-b.key $P/client.key
mnt $PXIP $PXPORT $TLS /mnt/iR; rc=$?
LR=$(ls1 /mnt/iR); echo "  root after rotating a→b in place: $LR"
if echo "$LR" | grep -qw ws-c; then answer Q5 "yes: tlshd presented b's certificate without a restart"
elif echo "$LR" | grep -qw ws-a; then answer Q5 "NO: tlshd still presented a's certificate; the client DaemonSet must restart tlshd on rotation"
else answer Q5 "UNDECIDED: mount rc=$rc, root [$LR]"; fi
drop /mnt/iR $PXIP
cp $P/client-a.crt $P/client.crt; cp $P/client-a.key $P/client.key; sudo systemctl restart tlshd

echo "== Q3 the hub's kubelet probe"
HC=$(K -n $NS get pod -l chert.us/share=ws-c -o jsonpath='{.items[0].metadata.name}')
HNODE=$(K -n $NS get pod $HC -o jsonpath='{.spec.nodeName}')
CID=$(docker exec $HNODE crictl ps --name hub --label io.kubernetes.pod.name=$HC -q | head -1)
HPID=$(docker exec $HNODE crictl inspect -o go-template --template '{{.info.pid}}' $CID)
ready() { K -n $NS get pod $HC -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}'; }
echo "  hub $HC on $HNODE pid $HPID ready=$(ready)"
docker exec $HNODE nsenter -t $HPID -n iptables -I INPUT -p tcp --dport 2049 -j REJECT --reject-with tcp-reset
went=no
for _ in $(seq 18); do [ "$(ready)" = False ] && { went=yes; break; }; sleep 2; done
docker exec $HNODE nsenter -t $HPID -n iptables -D INPUT -p tcp --dport 2049 -j REJECT --reject-with tcp-reset
back=no
for _ in $(seq 30); do [ "$(ready)" = True ] && { back=yes; break; }; sleep 2; done
if [ $went = yes ]; then answer Q3 "yes: the probe reaches the hub itself (REJECT in its netns made it NotReady)"; else answer Q3 "NO: the pod stayed Ready with 2049 rejected — the probe is answered elsewhere (vacuous)"; fi
check "Q3 control: the hub is Ready again once the rule is gone" '[ $back = yes ]'

echo "== hub lockdown: the AuthorizationPolicy"
HIP=$(K -n $NS get pod -l chert.us/share=ws-b -o jsonpath='{.items[0].status.podIP}')
# An RPC NULL call to NFSv4 (octal: busybox printf); a served one answers 28 bytes.
NULLCALL='\200\000\000\050\000\000\000\001\000\000\000\000\000\000\000\002\000\001\206\243\000\000\000\004\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000\000'
rpcnull() {  # $1 serviceAccount → bytes answered
  K -n $OPNS run np-$RANDOM --rm -i --restart=Never --image=busybox:1.36 \
    --labels="app.kubernetes.io/name=flint-lite-operator-nfs-proxy,app.kubernetes.io/instance=flint-lite-operator" \
    --overrides="{\"spec\":{\"serviceAccountName\":\"$1\"}}" --command -- \
    sh -c "sleep 3; printf '$NULLCALL' | nc -w 5 $HIP 2049 | wc -c" 2>/dev/null | grep -E '^[0-9]+$' | tail -1
}
LOOK=$(rpcnull default); OWN=$(rpcnull flint-lite-operator-nfs-proxy)
echo "  NULL reply bytes: proxy labels + default SA: ${LOOK:-none}; proxy labels + the proxy's SA: ${OWN:-none}"
check "a pod with the proxy's LABELS but another ServiceAccount gets no NFS reply" '[ "${LOOK:-0}" = 0 ]'
check "control: the proxy's ServiceAccount gets the reply" '[ "${OWN:-0}" = 28 ]'

echo "== Q4 control arm: the proxy WITHOUT the bypass"
values false
helm upgrade flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm2.log 2>&1 || { tail $OUT/helm2.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null
read PXNODE PXIP <<<"$(pxnode)"
PXPOD=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].metadata.name}')
check "control arm: the new proxy pod has no bypass annotation" '[ -z "$(K -n $OPNS get pod $PXPOD -o jsonpath="{.metadata.annotations.ambient\.istio\.io/bypass-inbound-capture}")" ]'
mnt $PXIP $PXPORT $TLS /mnt/iZ; rc=$?
check "control arm: path A still mounts with the proxy's inbound captured" '[ $rc = 0 ] && ls1 /mnt/iZ | grep -qw ws-a'
c0=$(connects /mnt/iZ); zrestart
r=$(sudo timeout 180 cat /mnt/iZ/ws-a/f 2>&1)
c1=$(connects /mnt/iZ)
echo "  connect_count $c0 → $c1; read [$r]"
CAPTURED_RESET=$([ "$c1" != "$c0" ] && echo yes || echo no)
if [ $CAPTURED_RESET = yes ] && [ $BYPASS_RESET = no ]; then
  answer Q4 "yes: a ztunnel restart reset the client's connection when captured ($c0→$c1) and not with the bypass — the bypass matters"
elif [ $CAPTURED_RESET = no ] && [ $BYPASS_RESET = no ]; then
  answer Q4 "NO reset either way — the bypass buys nothing against ztunnel restarts here"
else
  answer Q4 "UNEXPECTED: reset with bypass=$BYPASS_RESET, captured=$CAPTURED_RESET"
fi
check "control arm: I/O recovers after the restart" '[ "$r" = "$MARK" ]'

K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy > $OUT/proxy.log 2>&1
K -n $OPNS logs deploy/flint-lite-operator > $OUT/operator.log 2>&1
K -n istio-system logs ds/ztunnel --all-containers --tail=-1 > $OUT/ztunnel.log 2>&1
echo "--- answers"; cat $OUT/answers.txt
echo "RESULT: $ok passed, $bad failed"
