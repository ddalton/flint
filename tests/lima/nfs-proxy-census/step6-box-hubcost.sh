#!/bin/bash
# nfs-proxy step 6, phase A on the box (docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md):
# what ONE REAL HUB costs — memory, CPU, state.db, start-to-Ready — at
# 1k, 5k and 10k files, idle and under a light metadata load. kind with
# the real hub image, RustFS for the bucket, the proxy in front, the host
# kernel as the client. Not flint-spdk (that is the AWS session): the hub
# process is the same, its I/O path is not.
#   bash step6-box-hubcost.sh      # PER_TIER=10 TIERS="1000 5000 10000"; KEEP=1
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
REPO=${REPO:-$HOME/nfs-proxy-census/flint}
CHART=$REPO/flint-lite-operator-chart
CLUSTER=flint-step6
OPNS=flint-system; NS=ws; S3NS=s3
TAG=${TAG:-step5-dev}
OPIMG=flint-lite-operator:$TAG; HUBIMG=flint-pnfs:$TAG
PER_TIER=${PER_TIER:-10}; TIERS=${TIERS:-"1000 5000 10000"}
DATA=/mnt/nvme2/step6          # PVCs and the bucket live on NVMe, not the 98G root
OUT=$HOME/nfs-proxy-step6-A; MNT=/mnt/px6
HEALTH=${HEALTH:-8080}           # render::HEALTH_PORT
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export TMPDIR=$HOME/tmp
# ONE run at a time: an earlier run survived a parse error in a function
# body, the next one's kind delete and umount hit it mid-seed, and the two
# shared a log — a "vanishing workspace" that was the rig, not the proxy.
exec 9>$HOME/.$CLUSTER.lock
flock -n 9 || { echo "another $CLUSTER run holds $HOME/.$CLUSTER.lock"; exit 1; }
# Our own kubeconfig: the box's ~/.kube/config is shared with other
# sessions' kind clusters, and a context switch there would retarget us.
export KUBECONFIG=$HOME/.kube/$CLUSTER.config
rm -rf $OUT; mkdir -p $OUT $TMPDIR
ok=0; bad=0
check() { if eval "$2"; then echo "PASS $1"; ok=$((ok+1)); else echo "FAIL $1"; bad=$((bad+1)); fi; }
K() { kubectl "$@"; }
cleanup() {
  unmount_hard $MNT
  if [ "${KEEP:-0}" != 1 ]; then kind delete cluster --name $CLUSTER >/dev/null 2>&1; sudo rm -rf $DATA; fi
}
trap cleanup EXIT INT TERM
for i in $OPIMG $HUBIMG rustfs/rustfs:latest cgr.dev/chainguard/minio-client:latest-dev; do
  docker image inspect $i >/dev/null 2>&1 || { echo "missing image $i"; exit 1; }
done

echo "== cluster (PVCs and bucket on $DATA)"
kind delete cluster --name $CLUSTER >/dev/null 2>&1
sudo rm -rf $DATA; sudo mkdir -p $DATA/lp $DATA/s3
sudo chmod 777 $DATA/s3   # RustFS runs as a non-root user (run 0: EACCES, CrashLoop)
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
K -n $S3NS rollout status deploy/minio --timeout=180s >/dev/null && K -n $S3NS wait --for=condition=Ready pod -l app=minio --timeout=120s >/dev/null || { echo "rustfs not ready"; K -n $S3NS logs -l app=minio --tail=3; exit 1; }
mc() { K -n $S3NS run mc-$RANDOM --rm -i --restart=Never --image=cgr.dev/chainguard/minio-client:latest-dev --image-pull-policy=Never --command -- \
  sh -c "for i in \$(seq 1 60); do mc alias set m http://minio.$S3NS.svc:9000 drill drillsecret >/dev/null 2>&1 && break; sleep 2; done; $1" 2>/dev/null | grep -v "^pod "; }
mc "mc mb --ignore-existing m/fleet" | tail -1
K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=drill --from-literal=AWS_SECRET_ACCESS_KEY=drillsecret >/dev/null

echo "== operator + proxy"
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
EOF
helm install flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml > $OUT/helm.log 2>&1 || { tail $OUT/helm.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || exit 1

HUBS=""
for n in $TIERS; do for i in $(seq 1 $PER_TIER); do HUBS="$HUBS h$n-$i"; done; done
for h in $HUBS; do
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
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
st() { K -n $NS get flintshare $1 -o jsonpath="{.status.$2}" 2>/dev/null; }
for h in $HUBS; do
  for _ in $(seq 1 120); do [ "$(phase $h)" = Ready ] && [ -n "$(st $h serverId)" ] && break; sleep 3; done
done
NREADY=$(for h in $HUBS; do phase $h; echo; done | grep -c '^Ready$')
check "all $(echo $HUBS | wc -w) hubs Ready" '[ "$NREADY" = "$(echo $HUBS | wc -w)" ]'
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || exit 1

echo "== mount and seed (files of 1-64 KiB, log-uniform)"
PORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
PXNODE=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
PXIP=$(K get node $PXNODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
sudo mkdir -p $MNT
sudo timeout 60 mount -t nfs4 -o nfsvers=4.2,proto=tcp,hard,timeo=50,port=$PORT $PXIP:/ $MNT || { echo "mount failed"; exit 1; }
# The proxy picks shares up from its FlintShare watch on a timer: seed only
# once / lists every hub (run 0 created into the read-only pseudo-root).
want=$(echo $HUBS | tr ' ' '\n' | sort | tr '\n' ' ')
for _ in $(seq 1 60); do [ "$(ls $MNT | sort | tr '\n' ' ')" = "$want" ] && break; sleep 3; done
check "/ lists every hub before seeding" '[ "$(ls $MNT | sort | tr "\n" " ")" = "$want" ]'
T0=$(date +%s)
for h in $HUBS; do
  n=${h#h}; n=${n%-*}
  sudo python3 - $MNT/$h $n <<'P' &
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
done
wait
SEED_S=$(( $(date +%s) - T0 ))
echo "seeded $(echo $HUBS | wc -w) hubs in ${SEED_S}s"
for n in $TIERS; do
  h=h$n-1; c=$(sudo find $MNT/$h -type f | wc -l)
  check "tier $n: $h holds $n files through the proxy" '[ "$c" = "$n" ]'
done

echo "== flushed to the bucket (rpoClean) before measuring"
pod() { K -n $NS get pod -l chert.us/share=$1 -o jsonpath='{.items[0].metadata.name}'; }
rpo() { K -n $NS exec $(pod $1) -- curl -s http://127.0.0.1:$HEALTH/status | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("rpoClean"))' 2>/dev/null; }
for _ in $(seq 1 60); do
  NCLEAN=$(for h in $HUBS; do rpo $h; done | grep -c True); [ "$NCLEAN" = "$(echo $HUBS | wc -w)" ] && break; sleep 10
done
check "every hub reports rpoClean (its files are in the bucket)" '[ "$NCLEAN" = "$(echo $HUBS | wc -w)" ]'
for n in $TIERS; do
  o=$(mc "mc ls --recursive m/fleet/h$n-1/ | grep -vc '/\.flint/'" | tail -1)
  echo "bucket objects under h$n-1/ (excluding .flint/): $o"
done
# What the bucket holds besides the files (the smoke run counted N+3).
mc "mc ls --recursive m/fleet/h$(echo $TIERS | awk '{print $1}')-1/ | grep -v '/d[0-9][0-9][0-9]/f[0-9]*\$'" | head -10 | tee $OUT/bucket-extras.txt

# Per-hub sample: RSS (MiB), CPU (millicores over the window), state.db size (KiB).
cpu_us() { K -n $NS exec $(pod $1) -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }
sample() {  # $1 label, $2 window secs, hubs...
  local label=$1 win=$2; shift 2
  declare -A c0
  for h in "$@"; do c0[$h]=$(cpu_us $h); done
  sleep $win
  for h in "$@"; do
    local p; p=$(pod $h)
    # The container runs flint-pnfs-mds directly (the operator's command), so PID 1 is the hub.
    local rss; rss=$(K -n $NS exec $p -- sh -c 'grep VmRSS /proc/1/status; cat /proc/1/comm' 2>/dev/null | awk '/VmRSS/ {r=int($2/1024)} /flint-pnfs-mds/ {ok=1} END {print ok ? r : "not-the-hub"}')
    local db; db=$(K -n $NS exec $p -- sh -c "du -k \$(find / -name state.db -not -path '/proc/*' 2>/dev/null | head -1)" 2>/dev/null | awk '{print $1}')
    local c1; c1=$(cpu_us $h)
    local mc; mc=$(( (c1 - ${c0[$h]}) / win / 1000 ))
    echo -e "$label\t$h\t${h%-*}\trss_mib=$rss\tcpu_m=$mc\tstatedb_kib=${db:-?}"
  done
}
echo "== idle: ${IDLE:-600} s settle, then a 120 s window"
sleep ${IDLE:-600}
sample idle 120 $HUBS | tee $OUT/idle.tsv
echo "== under load: a metadata walk + small writes on the 10k tier, 180 s window"
LOAD=""; for i in $(seq 1 $PER_TIER); do LOAD="$LOAD h$(echo $TIERS | awk '{print $NF}')-$i"; done
for h in $LOAD; do
  sudo timeout 200 python3 - $MNT/$h <<'P' &
import os, sys, time
root = sys.argv[1]
end = time.time() + 195
i = 0
while time.time() < end:
    for d, _, files in os.walk(root):
        for f in files[:20]:
            os.stat(os.path.join(d, f))
    p = os.path.join(root, f"load-{i % 50}")
    with open(p, "wb") as f:
        f.write(b"x" * 4096)
    i += 1
    time.sleep(0.2)
P
done
sleep 10
sample load 180 $LOAD | tee $OUT/load.tsv
wait
echo "== start to Ready: restart the 10k tier's hubs (PVC kept, no import)"
for h in $LOAD; do K -n $NS delete pod $(pod $h) --wait=false >/dev/null; done
sleep 5
for h in $LOAD; do
  for _ in $(seq 1 120); do
    p=$(pod $h); r=$(K -n $NS get pod $p -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
    [ "$r" = True ] && break; sleep 2
  done
  K -n $NS get pod $(pod $h) -o json | python3 -c '
import json, sys
from datetime import datetime
p = json.load(sys.stdin); s = p["status"]
f = lambda t: datetime.fromisoformat(t.replace("Z", "+00:00"))
ready = [c for c in s["conditions"] if c["type"] == "Ready"][0]["lastTransitionTime"]
print(f"'$h'\tstart_to_ready_s={(f(ready) - f(s["startTime"])).total_seconds():.0f}")'
done | tee $OUT/startup.tsv
echo "== context"
for d in flint-lite-operator flint-lite-operator-nfs-proxy; do
  echo "$d: $(K -n $OPNS exec deploy/$d -- sh -c 'grep VmRSS /proc/1/status' 2>/dev/null | awk '{print int($2/1024)" MiB"}')"
done | tee $OUT/context.txt
echo "== summary per tier (idle / load)"
python3 - $OUT/idle.tsv $OUT/load.tsv <<'P' | tee $OUT/summary.txt
import re, sys, statistics as st
for path in sys.argv[1:]:
    rows = {}
    for line in open(path):
        f = line.rstrip("\n").split("\t")
        if len(f) < 6: continue
        kv = dict(x.split("=") for x in f[3:])
        rows.setdefault(f[2], []).append(kv)
    for tier, rs in sorted(rows.items()):
        def rng(k):
            v = [int(r[k]) for r in rs if r[k].lstrip("-").isdigit()]
            return f"{min(v)}-{max(v)} (median {st.median(v):.0f})" if v else "?"
        print(f"{path.split('/')[-1][:-4]:5} {tier:7} n={len(rs):2}  rss_mib {rng('rss_mib')}  cpu_m {rng('cpu_m')}  statedb_kib {rng('statedb_kib')}")
P
sudo umount $MNT 2>/dev/null
echo "RESULT: $ok passed, $bad failed"
