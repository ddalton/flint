#!/bin/bash
# step 6, a probe from phase A's finding: an IDLE hub's CPU grew with its
# file count (4 / 8 / 11 millicores at 1k / 5k / 10k files). Every rig so
# far set `settings.flushFloorSecs: 3`; the default is 60. A/B on the
# phase B cluster (kept): two fresh 10k-file hubs, one per arm, same
# seed, sampled idle three times. Also records the operator's /status
# poll rate, the other thing an idle hub answers.
#   bash step6-box-idlecpu.sh    (needs step6-box-wake.sh run with KEEP=1)
set -u
CLUSTER=flint-step6b
export PATH=$HOME/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH
export KUBECONFIG=$HOME/.kube/$CLUSTER.config
exec 9>$HOME/.$CLUSTER.lock
flock -n 9 || { echo "another $CLUSTER run holds the lock"; exit 1; }
NS=ws; S3NS=s3; OPNS=flint-system; MNT=/mnt/px6c; OUT=$HOME/nfs-proxy-step6-idlecpu
rm -rf $OUT; mkdir -p $OUT
K() { kubectl "$@"; }
pod() { K -n $NS get pod -l chert.us/share=$1 -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }
phase() { K -n $NS get flintshare $1 -o jsonpath='{.status.phase}' 2>/dev/null; }
for arm in floor3 default; do
  extra=""; [ $arm = floor3 ] && extra="  settings: { flushFloorSecs: 3 }"
  cat <<EOF | K apply -f - >/dev/null
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: cpu-$arm, namespace: $NS }
spec:
  bucket: fleet
  keyPrefix: cpu-$arm/
  endpoint: http://minio.$S3NS.svc:9000
  region: us-east-1
  credentialsSecretRef: s3
  idle: {}
  persistence: { size: 2Gi }
$extra
EOF
done
for s in cpu-floor3 cpu-default; do for _ in $(seq 1 150); do [ "$(phase $s)" = Ready ] && break; sleep 2; done; done
PORT=$(K -n $OPNS get svc flint-lite-operator-nfs-proxy -o jsonpath='{.spec.ports[0].nodePort}')
PXNODE=$(K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].spec.nodeName}')
PXIP=$(K get node $PXNODE -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
sudo mkdir -p $MNT; sudo timeout 60 mount -t nfs4 -o nfsvers=4.2,port=$PORT $PXIP:/ $MNT || exit 1
for _ in $(seq 1 60); do ls $MNT | grep -q cpu-default && ls $MNT | grep -q cpu-floor3 && break; sleep 3; done
for s in cpu-floor3 cpu-default; do
  sudo python3 - $MNT/$s 10000 <<'P' &
import math, os, random, sys
root, n = sys.argv[1], int(sys.argv[2])
rnd = random.Random("same-seed")
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
echo "seeded: floor3 $(sudo find $MNT/cpu-floor3 -type f | wc -l), default $(sudo find $MNT/cpu-default -type f | wc -l)"
sudo umount $MNT
echo "flush: the default arm needs >= 60 s per file; waiting 5 min, then 5 min idle"
sleep 600
cpu_us() { K -n $NS exec $(pod $1) -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }
rpo() { K -n $NS exec $(pod $1) -- curl -s http://127.0.0.1:8080/status 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("rpoClean"))' 2>/dev/null; }
echo "rpoClean: floor3 $(rpo cpu-floor3), default $(rpo cpu-default)"
for i in 1 2 3; do
  a0=$(cpu_us cpu-floor3); b0=$(cpu_us cpu-default); sleep 120; a1=$(cpu_us cpu-floor3); b1=$(cpu_us cpu-default)
  echo -e "sample $i\tfloor3_cpu_m=$(( (a1 - a0) / 120000 ))\tdefault_cpu_m=$(( (b1 - b0) / 120000 ))"
done | tee $OUT/samples.tsv
# How often the operator asks each hub for /status (the ladder's poll):
for s in cpu-floor3 cpu-default; do
  n=$(K -n $NS logs $(pod $s) --since=10m 2>/dev/null | grep -c "GET /status" || true)
  echo "$s: /status lines logged in 10 min: $n (0 = the hub does not log them)"
done | tee -a $OUT/samples.tsv
