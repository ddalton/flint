#!/bin/bash
# nfs-proxy step 5 on kind (the box): the scale prerequisites, end to end.
#   - the FLEET IDLE DEFAULT (nfsProxy.idleDefaults): shares with no
#     spec.idle suspend, and hibernate when they have a bucket;
#     `spec.idle: {}` opts out; a share without a bucket only suspends;
#   - HIB-1: a share a client holds a LOCK on (through the proxy) is NOT
#     hibernated — the operator goes back to suspended and keeps the disk —
#     while a share the same mount only listed and wrote (a lease, no
#     state) IS, and wakes from the bucket through the proxy;
#   - the lock survives the verify wake (the proxy's keepalive re-attach);
#   - a hibernated share is its CR alone (Deployment, Service, ConfigMap
#     deleted), and wakes through a RESTARTED proxy, which has only the
#     address the CR implies;
#   - restartOnTgtRestart: a rollout of the csi-node DaemonSet (a stand-in
#     with an `spdk-tgt` container) restarts every running hub on that
#     node whose PVC is in the listed class, and no other.
#   bash step5-kind.sh           # KEEP=1 leaves the cluster up
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CARGO_DIR=$REPO/spdk-csi-driver
CHART=$REPO/flint-lite-operator-chart
CLUSTER=flint-step5
OPNS=flint-system; NS=workspaces; S3NS=s3
TAG=step5-dev
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
OUT=$HOME/nfs-proxy-step5; MNT=/mnt/px5
SUSPEND=60; HIBERNATE=120
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp   # snap docker cannot read outside $HOME
rm -rf $OUT; mkdir -p $OUT $TMPDIR
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
K() { kubectl "$@"; }
HOLDER=
cleanup() {
  [ -n "$HOLDER" ] && sudo kill $HOLDER 2>/dev/null
  sudo umount -f -l $MNT 2>/dev/null
  [ "${KEEP:-0}" = 1 ] || kind delete cluster --name $CLUSTER >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

echo "== images"
R=$CARGO_DIR/target/x86_64-unknown-linux-musl/release
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
for i in rustfs/rustfs:latest cgr.dev/chainguard/minio-client:latest-dev busybox:1.36; do
  docker image inspect $i >/dev/null 2>&1 || docker pull -q $i >/dev/null || { echo "pull $i failed"; exit 1; }
done

echo "== cluster"
kind delete cluster --name $CLUSTER >/dev/null 2>&1
printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n  - role: worker\n' > $OUT/kind.yaml
kind create cluster --name $CLUSTER --config $OUT/kind.yaml --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
for i in $HUBIMG $OPIMG rustfs/rustfs:latest cgr.dev/chainguard/minio-client:latest-dev busybox:1.36; do
  kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || { echo "kind load $i failed"; exit 1; }
done
K create ns $OPNS >/dev/null; K create ns $NS >/dev/null; K create ns $S3NS >/dev/null

echo "== S3 (RustFS) and a second storage class"
cat <<EOF | K apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio, namespace: $S3NS }
spec:
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      containers:
        - name: minio
          image: rustfs/rustfs:latest
          imagePullPolicy: IfNotPresent
          env:
            - { name: RUSTFS_ACCESS_KEY, value: drill }
            - { name: RUSTFS_SECRET_KEY, value: drillsecret }
            - { name: RUSTFS_VOLUMES, value: /data }
          ports: [{ containerPort: 9000 }]
          volumeMounts: [{ name: data, mountPath: /data }, { name: logs, mountPath: /logs }]
      volumes: [{ name: data, emptyDir: {} }, { name: logs, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: minio, namespace: $S3NS }
spec: { selector: { app: minio }, ports: [{ port: 9000 }] }
---
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: { name: other }
provisioner: rancher.io/local-path
volumeBindingMode: WaitForFirstConsumer
reclaimPolicy: Delete
EOF
K -n $S3NS rollout status deploy/minio --timeout=180s >/dev/null || { echo "rustfs not ready"; exit 1; }
K -n $S3NS run mb --rm -i --restart=Never --image=cgr.dev/chainguard/minio-client:latest-dev --image-pull-policy=Never --command -- \
  sh -c 'until mc alias set m http://minio.s3.svc:9000 drill drillsecret >/dev/null; do sleep 2; done; mc mb --ignore-existing m/fleet' 2>&1 | tail -1
K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=drill --from-literal=AWS_SECRET_ACCESS_KEY=drillsecret >/dev/null

echo "== the csi-node stand-in (BEFORE any hub: a real node's tgt is always older than its hubs)"
cat <<EOF | K apply -f - >/dev/null
apiVersion: apps/v1
kind: DaemonSet
metadata: { name: flint-csi-node, namespace: $OPNS }
spec:
  selector: { matchLabels: { app: flint-csi-node } }
  template:
    metadata: { labels: { app: flint-csi-node } }
    spec:
      tolerations: [{ operator: Exists }]
      containers:
        - { name: spdk-tgt, image: busybox:1.36, imagePullPolicy: Never, command: [sleep, "1000000"] }
EOF
K -n $OPNS rollout status ds/flint-csi-node --timeout=120s >/dev/null || { echo "stand-in DS not ready"; exit 1; }

echo "== install"
cat > $OUT/values.yaml <<EOF
image: { ref: "$OPIMG", pullPolicy: Never }
hubImage: "$HUBIMG"
hubImagePullPolicy: Never
replicas: 1
restartOnTgtRestart:
  enabled: true
  storageClasses: [standard]
nfsProxy:
  enabled: true
  idleDefaults: { suspendAfterSecs: $SUSPEND, hibernateAfterSecs: $HIBERNATE }
  service: { type: NodePort, externalTrafficPolicy: Local }
  logLevel: info
  identities:
    - name: host
      sources: ["172.16.0.0/12", "10.0.0.0/8", "192.168.0.0/16"]
      workspaces: ["ws-*"]
EOF
helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || { echo "operator not ready"; exit 1; }

share() {  # name bucket(yes|no) extra-spec-lines
  local b=""
  [ "$2" = yes ] && b="  bucket: fleet
  keyPrefix: $1/
  endpoint: http://minio.$S3NS.svc:9000
  region: us-east-1
  credentialsSecretRef: s3
  settings: { flushFloorSecs: 3 }"
  cat <<EOF | K apply -f - >/dev/null || echo "share $1 REFUSED"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $1, namespace: $NS }
spec:
$b
  persistence: { size: 1Gi${4:-} }
${3:-}
EOF
}
share ws-lock yes; share ws-lease yes; share ws-nobucket no
share ws-optout yes "  idle: {}"; share ws-other no "  idle: {}" ", storageClassName: other"
ALL="ws-lock ws-lease ws-nobucket ws-optout ws-other"
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
st() { K -n $NS get flintshare $1 -o jsonpath="{.status.$2}" 2>/dev/null; }
pvc() { K -n $NS get pvc -l chert.us/share=$1 -o name 2>/dev/null | head -1; }
pod() { K -n $NS get pod -l chert.us/share=$1 -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null; }
events() { K -n $NS get events --field-selector involvedObject.name=$1 -o jsonpath='{range .items[*]}{.reason} {end}' 2>/dev/null; }
for s in $ALL; do
  for _ in $(seq 1 90); do [ "$(phase $s)" = Ready ] && [ -n "$(st $s serverId)" ] && break; sleep 4; done
  echo "$s: phase=$(phase $s) serverId=$(st $s serverId)"
done
check "an explicit empty spec.idle survived admission (the opt-out)" '[ "$(K -n $NS get flintshare ws-optout -o jsonpath="{.spec.idle}")" = "{}" ]'
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || { echo "proxy not ready"; exit 1; }

echo "== mount; a lock on ws-lock, a written-and-closed file on ws-lease"
PORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
PXNODE=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
PXIP=$(K get node $PXNODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
sudo mkdir -p $MNT
sudo timeout 60 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,actimeo=3600,port=$PORT $PXIP:/ $MNT
check "mount" 'timeout 10 mountpoint -q $MNT'
echo lease-bytes | sudo timeout 60 tee $MNT/ws-lease/f >/dev/null
sudo python3 - $MNT/ws-lock/lk $OUT <<'P' & HOLDER=$!
import fcntl, os, sys, time
p, out = sys.argv[1], sys.argv[2]
fd = os.open(p, os.O_CREAT | os.O_RDWR, 0o644)
fcntl.lockf(fd, fcntl.LOCK_EX)
open(f"{out}/held", "w").close()
while not os.path.exists(f"{out}/go"):
    time.sleep(0.5)
try:
    os.pwrite(fd, b"after\n", 0); os.fsync(fd); r = "ok"
except OSError as e:
    r = f"errno {e.errno}"
open(f"{out}/res", "w").write(r)
time.sleep(3600)
P
for _ in $(seq 1 60); do [ -e $OUT/held ] && break; sleep 1; done
check "the holder took its lock through the proxy" '[ -e $OUT/held ]'

echo "== the ladder runs (suspend ${SUSPEND}s, hibernate ${HIBERNATE}s, then a verify of >= one hub lease)"
T0=$(date +%s); DEFERRED=; LEASED=
while [ $(( $(date +%s) - T0 )) -lt 900 ]; do
  [ -z "$DEFERRED" ] && echo "$(events ws-lock)" | grep -q HibernateDeferred && DEFERRED=$(( $(date +%s) - T0 ))
  [ -z "$LEASED" ] && [ "$(phase ws-lease)" = Hibernated ] && [ -z "$(pvc ws-lease)" ] && LEASED=$(( $(date +%s) - T0 ))
  [ -n "$DEFERRED" ] && [ -n "$LEASED" ] && break
  sleep 5
done
for s in $ALL; do echo "$s: phase=$(phase $s) pvc=$(pvc $s) events=[$(events $s)]"; done
echo "ws-lock deferred at ${DEFERRED:-never}s; ws-lease hibernated at ${LEASED:-never}s"
check "ws-lease (a lease, no state) hibernated: PVC gone" '[ -n "$LEASED" ]'
check "ws-lock (a lock held) was NOT hibernated: deferred, back to IdleSuspended" '[ -n "$DEFERRED" ] && [ "$(phase ws-lock)" = IdleSuspended ]'
check "ws-lock kept its PVC" '[ -n "$(pvc ws-lock)" ]'
check "ws-lock's deferral names the held state" 'K -n $NS get events --field-selector involvedObject.name=ws-lock,reason=HibernateDeferred -o jsonpath="{.items[*].message}" | grep -q "hold opens, locks or delegations"'
check "ws-nobucket (fleet default, no bucket) suspended and kept its PVC" '[ "$(phase ws-nobucket)" = IdleSuspended ] && [ -n "$(pvc ws-nobucket)" ] && ! echo "$(events ws-nobucket)" | grep -q Hibernate'
check "ws-optout (spec.idle: {}) never left Ready" '[ "$(phase ws-optout)" = Ready ] && ! echo "$(events ws-optout)" | grep -q IdleSuspended'

echo "== the lock outlived two hub restarts (suspend, verify wake): the proxy re-attached"
touch $OUT/go
for _ in $(seq 1 240); do [ -e $OUT/res ] && break; sleep 2; done
echo "holder's write: $(cat $OUT/res 2>/dev/null || echo none)"
check "the holder's write after the verify wake succeeded (its lock was kept)" '[ "$(cat $OUT/res 2>/dev/null)" = ok ]'

echo "== a hibernated share is its CR alone"
objs() { K -n $NS get deploy,svc,cm -l chert.us/share=$1 -o name 2>/dev/null | wc -l; }
for _ in $(seq 1 30); do [ "$(objs ws-lease)" = 0 ] && break; sleep 3; done
echo "ws-lease objects: $(objs ws-lease); ws-lock objects: $(objs ws-lock)"
check "hibernated ws-lease has no Deployment, Service or ConfigMap left" '[ "$(objs ws-lease)" = 0 ] && echo "$(events ws-lease)" | grep -q ParkedAsCr'
check "suspended ws-lock keeps its objects (only a hibernated share is parked as CR)" '[ "$(objs ws-lock)" -ge 3 ]'
# Forget every remembered address: the wake below must dial the one the CR implies.
K -n $OPNS rollout restart deploy/flint-lite-operator-nfs-proxy >/dev/null
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null

echo "== ws-lease wakes from the bucket through the (restarted) proxy"
R=$(timeout 300 sudo cat $MNT/ws-lease/f 2>&1); echo "read: [$R] phase=$(phase ws-lease)"
check "the hibernated workspace woke and served its bytes from the bucket" '[ "$R" = lease-bytes ]'

echo "== an spdk-tgt restart (a csi-node rollout) restarts exactly the right hubs"
# Freeze the ladder first (spec.idle: {}), or its own suspends and wakes
# change pods during this leg and read as restarts (run 1).
for s in $ALL; do K -n $NS patch flintshare $s --type merge -p '{"spec":{"idle":{}}}' >/dev/null; done
sleep 20
declare -A BEFORE
for s in $ALL; do BEFORE[$s]=$(pod $s); done
RUNNING=""; for s in $ALL; do [ -n "${BEFORE[$s]}" ] && RUNNING="$RUNNING $s"; done
echo "running hubs before the roll:$RUNNING"
K -n $OPNS rollout restart ds/flint-csi-node >/dev/null
K -n $OPNS rollout status ds/flint-csi-node --timeout=120s >/dev/null
EXPECT=""; for s in $RUNNING; do [ $s != ws-other ] && EXPECT="$EXPECT $s"; done
settled() { for s in $EXPECT; do [ "$(pod $s)" != "${BEFORE[$s]}" ] && [ "$(phase $s)" = Ready ] || return 1; done; }
for _ in $(seq 1 60); do settled && break; sleep 5; done
for s in $ALL; do echo "$s: pod ${BEFORE[$s]:-none} -> $(pod $s) phase=$(phase $s) events=[$(events $s)]"; done
for s in $EXPECT; do
  check "$s (running, class standard) was restarted: new pod, HubRestarted" '[ "$(pod $s)" != "${BEFORE[$s]}" ] && echo "$(events $s)" | grep -q HubRestarted'
done
check "ws-other (class other) was NOT restarted" '[ "$(pod ws-other)" = "${BEFORE[ws-other]}" ]'
check "the restarted hubs are Ready again" 'settled'
# Counted from the operator's own mark lines. Run 2 summed the core
# events' `count`, which these events do not carry: 0 and 0.
restarts() { K -n $OPNS logs deploy/flint-lite-operator 2>/dev/null | grep -c "restarting the hub"; }
R1=$(restarts); sleep 70; R2=$(restarts); echo "restart marks: $R1 then $R2 (expected: one per restarted hub)"
check "convergent: one mark per restarted hub, none a minute later" '[ "$R1" = "$R2" ] && [ "$R1" = "$(echo $EXPECT | wc -w)" ]'

sudo kill $HOLDER 2>/dev/null; HOLDER=
sudo timeout 30 umount $MNT || sudo umount -f -l $MNT
K -n $OPNS logs deploy/flint-lite-operator > $OUT/operator.log 2>&1
K -n $OPNS logs deploy/flint-lite-operator-nfs-proxy > $OUT/proxy.log 2>&1
K -n $NS get events --sort-by=.lastTimestamp > $OUT/events.txt 2>&1
echo "RESULT: $ok passed, $bad failed"
