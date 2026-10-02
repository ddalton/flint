#!/bin/bash
# step 6 on AWS: what the build box cannot measure (plan:
# docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md, the AWS session).
#   E  real flint-spdk: the target's subsystem cap, and a real
#      flint-csi-node roll under 8 live hubs (a writer on each must lose
#      nothing; the 2026-10-02 run had no hub restart and one ~5 s stall).
#   C  the cross-node half of the proxy cost: a client on its own node,
#      the hub on another, direct vs through the proxy.
#
# Runs from the Mac against a trove cluster (all spot, control plane too):
#   KUBECONFIG=/tmp/trove-aws-kc-<name> TAG=<rc tag> bash step6-aws.sh setup|cap|roll|proxy
# Nodes: <name>-aws-1/2 are hub nodes, <name>-aws-3 the client/proxy node.
set -u
here=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$here/../../.." && pwd)
TAG=${TAG:?TAG=<release-candidate tag on Docker Hub>}
OUT=${OUT:-$here/results-aws-step6}
OPNS=flint-system; NS=ws; S3NS=s3
mkdir -p $OUT
K() { kubectl "$@"; }
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
nodes() { K get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | grep -v -- '-cp-'; }
HUBNODES=$(nodes | sort | head -2); CLIENTNODE=$(nodes | sort | sed -n 3p)
CSINS() { K get ds -A -l app=flint-csi-node -o jsonpath='{.items[0].metadata.namespace}' 2>/dev/null || K get ds -A -o jsonpath='{range .items[?(@.metadata.name=="flint-csi-node")]}{.metadata.namespace}{end}'; }
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
pod() { K -n $NS get pod -l chert.us/share=$1 -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
wait_ready() { for h in "$@"; do for _ in $(seq 1 180); do [ "$(phase $h)" = Ready ] && break; sleep 5; done; echo "  $h: $(phase $h)"; done; }
C() { K -n $NS exec nfsc -- "$@"; }   # the client pod
PXIP() { K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.clusterIP}'; }
PXPOD() { K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].metadata.name}'; }
cpu_us() { K -n $1 exec $2 -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }
# The proxy image has no shell, so its CPU is read from the node: a
# host-PID helper on the client node sums utime+stime of flint-nfs-proxy
# (clock ticks of 10 ms -> microseconds). A failed read is an error, not 0.
px_cpu_us() {
  K -n $NS exec hostpid -- sh -c 'p=$(pgrep -o -f "^/usr/local/bin/flint-nfs-proxy"); [ -n "$p" ] && awk "{print (\$14 + \$15) * 10000}" /proc/$p/stat' 2>/dev/null | grep . || echo ERR
}

shares() {  # $1 node (hostname), names... : flint-spdk disk, bucket-backed, ladder off
  local node=$1; shift
  for h in "$@"; do cat <<EOF
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
  nodeSelector: { kubernetes.io/hostname: $node }
  persistence: { size: 2Gi, storageClassName: flint-spdk }
EOF
  done | K apply -f - >/dev/null
}

case ${1:-} in
setup)
  echo "== nodes: hubs on $(echo $HUBNODES | tr '\n' ' '); client + proxy on $CLIENTNODE"
  for n in $HUBNODES; do K label node $n step6/role=hub --overwrite >/dev/null; done
  K label node $CLIENTNODE step6/role=client --overwrite >/dev/null
  if [ "${SKIP_CSI:-0}" != 1 ]; then
  echo "== flint-spdk from HEAD (images $TAG; spdk-tgt as the chart pins it)"
  read -r REL RNS < <(helm list -A -a -o json | python3 -c 'import json,sys; r=[x for x in json.load(sys.stdin) if x["chart"].startswith("flint-csi")]; print(r[0]["name"], r[0]["namespace"]) if r else print("- -")')
  if [ "$REL" = - ]; then  # gone: an earlier run uninstalled it
    [ -f $OUT/trove-csi-values.yaml ] || { echo "no flint CSI release and no saved trove values"; exit 1; }
    REL=flint-csi; RNS=flint-system
  fi
  # A FRESH install, not an upgrade: trove installs an old chart (1.43.0)
  # whose StorageClass provisioner differs (immutable), and the node
  # DaemonSet is OnDelete, so an upgrade leaves the old node pods running.
  # trove's values are kept (SPDK mode, hugepages); the pNFS server is off
  # (this rig does not use it, and its Pending pods block --wait).
  [ -f $OUT/trove-csi-values.yaml ] || helm get values $REL -n $RNS -o yaml > $OUT/trove-csi-values.yaml
  helm uninstall $REL -n $RNS --wait > $OUT/helm-csi.log 2>&1
  K -n $RNS delete pvc --all --wait=false >/dev/null 2>&1; K delete sc flint-spdk flint-pnfs flint-nfs >/dev/null 2>&1
  helm install $REL $REPO/flint-csi-driver-chart -n $RNS -f $OUT/trove-csi-values.yaml \
    --set images.flintCsiDriver.tag=$TAG --set pnfs.server.image.tag=$TAG --set pnfs.enabled=false --set pnfs.server.enabled=false \
    --wait --timeout 15m >> $OUT/helm-csi.log 2>&1 || { tail -5 $OUT/helm-csi.log; exit 1; }
  K -n $RNS get pods -o wide | tee $OUT/csi-pods.txt | head -12
  else RNS=$(CSINS); echo "== flint-spdk: kept (SKIP_CSI=1) in $RNS"; fi
  # trove does not initialize the NVMe disks: until each node's blobstore
  # exists, flint-spdk has 0 disks and every CreateVolume fails.
  for n in $HUBNODES $CLIENTNODE; do
    p=$(K -n $RNS get pod -l app=flint-csi-node --field-selector spec.nodeName=$n -o jsonpath='{.items[0].metadata.name}')
    # A fresh local port per node, and the reply must name THIS node: a
    # leftover port-forward once sent every call to the first node.
    lp=$((19100 + RANDOM % 800))
    K -n $RNS port-forward pod/$p $lp:9081 >/dev/null 2>&1 & pf=$!; sleep 3
    pci=$(curl -s -m 20 -X POST -H 'Content-Type: application/json' -d '{}' http://127.0.0.1:$lp/api/disks/uninitialized \
      | python3 -c 'import json,sys; j=json.load(sys.stdin); assert j.get("node")==sys.argv[1], j.get("node"); d=[x for x in j.get("uninitialized_disks",[]) if not x.get("is_system_disk")]; print(d[0]["pci_address"] if d else "")' $n) \
      || { echo "  $n: the agent that answered is not $n"; kill $pf; exit 1; }
    [ -n "$pci" ] && echo "  $n: init $pci: $(curl -s -m 120 -X POST -H 'Content-Type: application/json' -d "{\"pci_address\":\"$pci\"}" http://127.0.0.1:$lp/api/disks/initialize_blobstore)" \
      || echo "  $n: no uninitialized disk (already done)"
    kill $pf 2>/dev/null; wait $pf 2>/dev/null
  done
  echo "== RustFS bucket on $CLIENTNODE"
  K create ns $OPNS >/dev/null 2>&1; K create ns $NS >/dev/null 2>&1; K create ns $S3NS >/dev/null 2>&1
  cat <<EOF | K apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio, namespace: $S3NS }
spec:
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      nodeSelector: { step6/role: client }
      containers:
        - name: minio
          image: rustfs/rustfs:latest
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
EOF
  K -n $S3NS rollout status deploy/minio --timeout=300s >/dev/null || { echo "rustfs not ready"; exit 1; }
  K -n $S3NS run mc --rm -i --restart=Never --image=cgr.dev/chainguard/minio-client:latest-dev --command -- \
    sh -c 'for i in $(seq 1 60); do mc alias set m http://minio.s3.svc:9000 drill drillsecret >/dev/null 2>&1 && break; sleep 2; done; mc mb --ignore-existing m/fleet' 2>/dev/null | grep -v "^pod " | tail -1
  K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=drill --from-literal=AWS_SECRET_ACCESS_KEY=drillsecret >/dev/null 2>&1
  echo "== operator + proxy ($TAG) on $CLIENTNODE"
  cat > $OUT/op-values.yaml <<EOF
image: { ref: "dilipdalton/flint-lite-operator:$TAG" }
hubImage: "dilipdalton/flint-pnfs:$TAG"
nodeSelector: { step6/role: client }
nfsProxy:
  enabled: true
  idleDefaults: { suspendAfterSecs: 0, hibernateAfterSecs: 0 }
  nodeSelector: { step6/role: client }
  persistence: { storageClassName: flint-spdk }   # the cluster has no default class
  service: { type: ClusterIP }                     # clients are in-cluster; no LB controller here
  identities: [{ name: all, sources: ["0.0.0.0/0"], workspaces: ["*"] }]
EOF
  helm upgrade --install flint-lite-operator $REPO/flint-lite-operator-chart -n $OPNS -f $OUT/op-values.yaml --wait --timeout 10m > $OUT/helm-op.log 2>&1 \
    || { tail -5 $OUT/helm-op.log; exit 1; }
  K -n $OPNS get pods -o wide | tee $OUT/op-pods.txt
  echo "== the client pod (privileged, kernel NFS client) on $CLIENTNODE"
  cat <<EOF | K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: nfsc, namespace: $NS }
spec:
  nodeSelector: { step6/role: client }
  containers:
    - name: c
      image: alpine:3.20
      command: [sh, -c, "apk add --no-cache nfs-utils python3 coreutils >/dev/null && touch /ready && sleep infinity"]
      securityContext: { privileged: true }
EOF
  for _ in $(seq 1 60); do C test -f /ready 2>/dev/null && break; sleep 3; done
  C test -f /ready && echo "client ready" || { echo "client pod not ready"; exit 1; }
  ;;

cap)
  RNS=$(CSINS)
  echo "== phase E1: the SPDK target's subsystem cap and use, per hub node" | tee $OUT/E-cap.txt
  for n in $HUBNODES; do
    p=$(K -n $RNS get pod -l app=flint-csi-node --field-selector spec.nodeName=$n -o jsonpath='{.items[0].metadata.name}')
    R() { K -n $RNS exec $p -c spdk-tgt -- sh -c "python3 /usr/local/scripts/rpc.py $1" 2>&1; }
    { echo "-- $n ($p)"
      echo "nvmf framework config:"; R "framework_get_config nvmf" | head -40
      echo "subsystems now: $(R nvmf_get_subsystems | python3 -c 'import json,sys; print(len(json.load(sys.stdin)))' 2>&1)"
      echo "transports:"; R nvmf_get_transports | head -30
    } | tee -a $OUT/E-cap.txt
  done
  ;;

roll)
  RNS=$(CSINS); E="e1 e2 e3 e4 e5 e6 e7 e8"
  echo "== phase E2: a real flint-csi-node roll under 8 live hubs" | tee $OUT/E-roll.txt
  set -- $HUBNODES
  shares $1 e1 e2 e3 e4; shares $2 e5 e6 e7 e8
  wait_ready $E | tee -a $OUT/E-roll.txt
  C sh -c 'mkdir -p /mnt/px; mountpoint -q /mnt/px || mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50 '"$(PXIP)"':/ /mnt/px'
  for _ in $(seq 1 40); do [ "$(C sh -c 'ls /mnt/px | tr "\n" " "')" = "e1 e2 e3 e4 e5 e6 e7 e8 " ] && break; sleep 3; done
  echo "proxy lists: $(C ls /mnt/px | tr '\n' ' ')" | tee -a $OUT/E-roll.txt
  # One writer per hub: a numbered record per line, fsync'd, ~4/s. The
  # writer records the last number fsync ACKNOWLEDGED; after the roll the
  # file must hold 1..that number with no gap and no duplicate.
  C sh -c 'cat > /w.py <<P
import os, sys, time
d = sys.argv[1]; path = f"/mnt/px/{d}/log"; ack = f"/ack-{d}"
n = 0; worst = 0.0
fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
while True:
    n += 1; t = time.time()
    os.write(fd, f"{n}\n".encode()); os.fsync(fd)
    worst = max(worst, time.time() - t)
    open(ack, "w").write(str(n)); open(ack + ".worst", "w").write(f"{worst:.2f}"); time.sleep(0.25)
P
pkill -f "^python3 /w.py"; sleep 1
for d in e1 e2 e3 e4 e5 e6 e7 e8; do rm -f /mnt/px/$d/log /ack-$d; nohup python3 /w.py $d > /w-$d.err 2>&1 & done'
  sleep 20
  acked() { C sh -c "cat /ack-$1 2>/dev/null"; }
  # bash 3.2 (macOS): no associative arrays; one variable per hub.
  get() { eval "echo \${$1_$2:-}"; }; put() { eval "$1_$2=\$3"; }
  for h in $E; do put A0 $h "$(acked $h)"; put POD0 $h "$(pod $h)"; done
  echo "before the roll: $(for h in $E; do echo -n "$h=$(get A0 $h) "; done)" | tee -a $OUT/E-roll.txt
  T0=$(date +%s)
  # The DaemonSet is OnDelete; flint's own roll controller replaces the
  # pods one node at a time. `rollout status` refuses OnDelete, so wait on
  # the DaemonSet's counters.
  K -n $RNS rollout restart ds/flint-csi-node >/dev/null; sleep 5
  for _ in $(seq 1 180); do
    read -r want upd rdy < <(K -n $RNS get ds flint-csi-node -o jsonpath='{.status.desiredNumberScheduled} {.status.updatedNumberScheduled} {.status.numberReady}')
    [ "${upd:-0}" = "$want" ] && [ "${rdy:-0}" = "$want" ] && break; sleep 5
  done
  echo "csi-node rolled in $(( $(date +%s) - T0 ))s" | tee -a $OUT/E-roll.txt
  # Every hub Ready again. On HEAD flint-spdk (2026-10-02) the hubs ride
  # through the roll on their SAME pods (one ~5 s fsync stall each), which
  # is why the operator's restart-on-roll (noderoll) was removed. Which
  # pod each hub ends on is recorded; the data checks must hold.
  for _ in $(seq 1 120); do
    all=1; for h in $E; do [ "$(phase $h)" = Ready ] || all=0; done
    [ $all = 1 ] && break; sleep 5
  done
  echo "hubs back after $(( $(date +%s) - T0 ))s: $(for h in $E; do echo -n "$h=$(phase $h) "; done)" | tee -a $OUT/E-roll.txt
  sleep 30
  for h in $E; do put A1 $h "$(acked $h)"; done
  sleep 10
  echo "after the roll: $(for h in $E; do echo -n "$h=$(get A1 $h) "; done)" | tee -a $OUT/E-roll.txt
  echo "longest fsync stall per writer (s): $(for h in $E; do echo -n "$h=$(C cat /ack-$h.worst 2>/dev/null) "; done)" | tee -a $OUT/E-roll.txt
  C sh -c 'pkill -f "^python3 /w.py"'; sleep 2
  for h in $E; do
    echo "$h: pod $( [ "$(pod $h)" = "$(get POD0 $h)" ] && echo SAME || echo NEW ), longest fsync stall $(C cat /ack-$h.worst)s" | tee -a $OUT/E-roll.txt
    check "$h: its writer resumed after the roll" '[ "$(get A1 $h)" -gt "$(get A0 $h)" ]'
    last=$(acked $h)
    gap=$(C python3 -c "
xs = [int(l) for l in open('/mnt/px/$h/log') if l.strip()]
want = list(range(1, len(xs) + 1))
print('ok' if xs == want and len(xs) >= $last else f'BAD n={len(xs)} acked=$last first_break={next((i for i,(a,b) in enumerate(zip(xs,want)) if a!=b), None)}')")
    check "$h: records 1..N with no gap or repeat, N >= last acknowledged ($last): $gap" '[ "$gap" = ok ]'
  done
  for h in $E; do echo "$h: $(K -n $NS get events --field-selector involvedObject.name=$h -o jsonpath='{range .items[*]}{.reason} {end}')"; done >> $OUT/E-roll.txt
  echo "RESULT: $ok passed, $bad failed" | tee -a $OUT/E-roll.txt
  ;;

proxy)
  SMALL=${SMALL:-2000}; SEQ_MIB=${SEQ_MIB:-512}; FILES=${FILES:-5000}
  set -- $HUBNODES
  echo "== phase C, cross-node: hub on $1, client + proxy on $CLIENTNODE" | tee $OUT/C-run.txt
  shares $1 c1; wait_ready c1 | tee -a $OUT/C-run.txt
  cat <<EOF | K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: hostpid, namespace: $NS }
spec:
  nodeSelector: { step6/role: client }
  hostPID: true
  containers: [{ name: c, image: alpine:3.20, command: [sleep, infinity], securityContext: { privileged: true } }]
EOF
  K -n $NS wait --for=condition=Ready pod/hostpid --timeout=120s >/dev/null || { echo "hostpid helper not ready"; exit 1; }
  echo "proxy CPU now: $(px_cpu_us) us" | tee -a $OUT/C-run.txt
  HUB=$(pod c1); HUBIP=$(K -n $NS get pod $HUB -o jsonpath='{.status.podIP}')
  C sh -c 'mkdir -p /mnt/x; mountpoint -q /mnt/x && umount -f /mnt/x; mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50 '"$(PXIP)"':/ /mnt/x'
  for _ in $(seq 1 40); do C test -d /mnt/x/c1 && break; sleep 3; done
  [ "${RESEED:-1}" = 1 ] && C python3 -c "
import os
for i in range($FILES):
    d = f'/mnt/x/c1/d{i // 100:03d}'
    if i % 100 == 0: os.makedirs(d, exist_ok=True)
    open(f'{d}/f{i:05d}', 'wb').write(b'x' * 4096)"
  C umount /mnt/x
  echo "seeded $FILES files" | tee -a $OUT/C-run.txt
  run_arm() {  # $1 arm, $2 rep
    local arm=$1 rep=$2 dir
    case $arm in
      direct) C mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,actimeo=0 $HUBIP:/ /mnt/x; dir=/mnt/x ;;
      proxy)  C mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,actimeo=0 $(PXIP):/ /mnt/x; dir=/mnt/x/c1 ;;
    esac
    local p0 h0 t0 t1 p1 h1 n
    W() {  # $1 label, $2 python
      p0=$(px_cpu_us); h0=$(cpu_us $NS $HUB); t0=$(date +%s.%N)
      n=$(C python3 -c "$2")
      t1=$(date +%s.%N); p1=$(px_cpu_us); h1=$(cpu_us $NS $HUB)
      [ "$p0" = ERR ] || [ "$p1" = ERR ] && { echo "proxy CPU read failed"; exit 1; }
      echo -e "$arm\trep=$rep\tw=$1\t$n\twall_s=$(echo "$t1 - $t0" | bc)\tproxy_cpu_ms=$(( (p1 - p0) / 1000 ))\thub_cpu_ms=$(( (h1 - h0) / 1000 ))"
    }
    W stat "
import os
n = 0
for _ in range(2):
    for d, _, fs in os.walk('$dir'):
        for f in fs: os.stat(os.path.join(d, f)); n += 1
print(f'ops={n}')"
    W create+unlink "
import os
d = '$dir/small-$arm-$rep'; os.makedirs(d, exist_ok=True)
for i in range($SMALL): open(f'{d}/s{i}', 'wb').write(b'x' * 4096)
for i in range($SMALL): os.unlink(f'{d}/s{i}')
os.rmdir(d); print(f'ops={2 * $SMALL}')"
    # No staged source file: the client node's root disk is 8 GiB and a
    # 512 MiB file there tipped it into disk pressure. Seeded pseudo-random
    # bytes are generated into dd's stdin; the checksum is computed as they go.
    W seq-write "
import subprocess, hashlib, random
r = random.Random($rep); h = hashlib.md5()
p = subprocess.Popen(['dd', 'of=$dir/big', 'bs=1M', 'oflag=direct', 'iflag=fullblock', 'status=none'], stdin=subprocess.PIPE)
for _ in range($SEQ_MIB):
    b = r.randbytes(1048576); h.update(b); p.stdin.write(b)
p.stdin.close(); assert p.wait() == 0
open('/big.md5', 'w').write(h.hexdigest()); print('mib=$SEQ_MIB')"
    W seq-read "
import subprocess, hashlib
p = subprocess.Popen(['dd', 'if=$dir/big', 'bs=1M', 'iflag=direct', 'status=none'], stdout=subprocess.PIPE)
h = hashlib.md5()
for b in iter(lambda: p.stdout.read(1048576), b''): h.update(b)
assert p.wait() == 0
print('mib=$SEQ_MIB intact=' + ('yes' if h.hexdigest() == open('/big.md5').read() else 'NO'))"
    C rm -f $dir/big; C umount /mnt/x
  }
  for r in 1 2 3; do run_arm direct $r; run_arm proxy $r; done | tee $OUT/C.tsv
  ;;
*) echo "usage: TAG=... bash step6-aws.sh setup|cap|roll|proxy"; exit 2 ;;
esac
