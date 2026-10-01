# Shared setup for the step 6 box rigs (B, C, D). Source it after setting
# CLUSTER, OUT, MNT; it defines K, check, the cluster/RustFS/operator
# bring-up, share creation, the proxy mount and the seeder. Phase A
# (step6-box-hubcost.sh) predates it and carries its own copy.
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CHART=$REPO/flint-lite-operator-chart
OPNS=flint-system; NS=ws; S3NS=s3
TAG=${TAG:-step5-dev}
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
DATA=/mnt/nvme2/$CLUSTER
HEALTH=8080
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp
exec 9>$HOME/.$CLUSTER.lock
flock -n 9 || { echo "another $CLUSTER run holds $HOME/.$CLUSTER.lock"; exit 1; }
export KUBECONFIG=$HOME/.kube/$CLUSTER.config
rm -rf $OUT; mkdir -p $OUT $TMPDIR
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
K() { kubectl "$@"; }
cleanup() {
  sudo umount -f -l $MNT 2>/dev/null
  if [ "${KEEP:-0}" != 1 ]; then kind delete cluster --name $CLUSTER >/dev/null 2>&1; sudo rm -rf $DATA; fi
}
trap cleanup EXIT INT TERM

up_cluster() {  # kind (PVCs + bucket on NVMe), RustFS, bucket `fleet`, secret `s3`
  for i in $OPIMG $HUBIMG rustfs/rustfs:latest cgr.dev/chainguard/minio-client:latest-dev; do
    docker image inspect $i >/dev/null 2>&1 || { echo "missing image $i"; exit 1; }
  done
  kind delete cluster --name $CLUSTER >/dev/null 2>&1
  sudo rm -rf $DATA; sudo mkdir -p $DATA/lp $DATA/s3; sudo chmod 777 $DATA/s3
  cat > $OUT/kind.yaml <<EOF
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
    extraMounts:
      - { hostPath: $DATA/lp, containerPath: /var/local-path-provisioner }
      - { hostPath: $DATA/s3, containerPath: /s3data }
EOF
  kind create cluster --name $CLUSTER --config $OUT/kind.yaml --wait 180s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
  for i in $OPIMG $HUBIMG rustfs/rustfs:latest cgr.dev/chainguard/minio-client:latest-dev; do
    kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || { echo "kind load $i failed"; exit 1; }
  done
  K create ns $OPNS >/dev/null; K create ns $NS >/dev/null; K create ns $S3NS >/dev/null
  cat <<EOF | K apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio, namespace: $S3NS }
spec:
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      nodeSelector: { kubernetes.io/hostname: $CLUSTER-worker }
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
      volumes: [{ name: data, hostPath: { path: /s3data } }, { name: logs, emptyDir: {} }]
---
apiVersion: v1
kind: Service
metadata: { name: minio, namespace: $S3NS }
spec: { selector: { app: minio }, ports: [{ port: 9000 }] }
EOF
  K -n $S3NS rollout status deploy/minio --timeout=180s >/dev/null && K -n $S3NS wait --for=condition=Ready pod -l app=minio --timeout=120s >/dev/null \
    || { echo "rustfs not ready"; K -n $S3NS logs -l app=minio --tail=3; exit 1; }
  mc "mc mb --ignore-existing m/fleet" | tail -1
  K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=drill --from-literal=AWS_SECRET_ACCESS_KEY=drillsecret >/dev/null
}
mc() { K -n $S3NS run mc-$RANDOM --rm -i --restart=Never --image=cgr.dev/chainguard/minio-client:latest-dev --image-pull-policy=Never --command -- \
  sh -c "for i in \$(seq 1 60); do mc alias set m http://minio.$S3NS.svc:9000 drill drillsecret >/dev/null 2>&1 && break; sleep 2; done; $1" 2>/dev/null | grep -v "^pod "; }

up_operator() {  # $1 = extra values yaml (appended)
  cat > $OUT/values.yaml <<EOF
image: { ref: "$OPIMG", pullPolicy: Never }
hubImage: "$HUBIMG"
hubImagePullPolicy: Never
replicas: 1
restartOnTgtRestart: { enabled: false }
nfsProxy:
  enabled: true
  idleDefaults: { suspendAfterSecs: 0, hibernateAfterSecs: 0 }
  service: { type: NodePort, externalTrafficPolicy: Local }
  identities: [{ name: host, sources: ["172.16.0.0/12", "10.0.0.0/8", "192.168.0.0/16"], workspaces: ["*"] }]
${1:-}
EOF
  helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
  K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || exit 1
}

shares() {  # names... : bucket-backed, ladder off (spec.idle: {}), 2Gi
  for h in "$@"; do
    cat <<EOF
---
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $h, namespace: $NS }
spec:
  bucket: fleet
  keyPrefix: $h/
  endpoint: http://minio.$S3NS.svc:9000
  region: us-east-1
  credentialsSecretRef: s3
  settings: { flushFloorSecs: 3 }
  idle: {}
  persistence: { size: 2Gi }
EOF
  done | K apply -f - >/dev/null
}
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
st() { K -n $NS get flintshare $1 -o jsonpath="{.status.$2}" 2>/dev/null; }
pod() { K -n $NS get pod -l chert.us/share=$1 -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
wait_ready() { for h in "$@"; do for _ in $(seq 1 150); do [ "$(phase $h)" = Ready ] && [ -n "$(st $h serverId)" ] && break; sleep 2; done; done; }
rpo() { K -n $NS exec $(pod $1) -- curl -s http://127.0.0.1:$HEALTH/status 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("rpoClean"))' 2>/dev/null; }

mount_proxy() {  # mounts / of the proxy at $MNT; waits until it lists the given hubs
  PORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
  PXNODE=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
  PXIP=$(K get node $PXNODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
  sudo mkdir -p $MNT
  sudo timeout 60 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$PORT${MOPTS:-} $PXIP:/ $MNT || { echo "mount failed"; exit 1; }
  local want; want=$(printf '%s\n' "$@" | sort | tr '\n' ' ')
  for _ in $(seq 1 60); do [ "$(ls $MNT | sort | tr '\n' ' ')" = "$want" ] && return 0; sleep 3; done
  echo "the proxy does not list: $want (got: $(ls $MNT | tr '\n' ' '))"; return 1
}
remount() { sudo timeout 30 umount $MNT || sudo umount -f -l $MNT; mount_proxy "$@"; }

seed() {  # $1 dir, $2 files — 1-64 KiB log-uniform, 100 per directory
  sudo python3 - "$1" "$2" <<'P'
import math, os, random, sys
root, n = sys.argv[1], int(sys.argv[2])
rnd = random.Random(root)
for i in range(n):
    d = os.path.join(root, f"d{i // 100:03d}")
    if i % 100 == 0:
        os.makedirs(d, exist_ok=True)
    size = int(math.exp(rnd.uniform(math.log(1024), math.log(65536))))
    with open(os.path.join(d, f"f{i:05d}"), "wb") as f:
        f.write(os.urandom(size))
P
}
