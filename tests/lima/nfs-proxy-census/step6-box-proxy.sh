#!/bin/bash
# nfs-proxy step 6, phase C on the box (docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md):
# what the PROXY costs per operation, against a direct mount of the same
# hub, and what mTLS adds. One hub (5k files), the host kernel client,
# three arms:
#   direct : the hub pod's IP (a host route through the kind worker)
#   proxy  : the proxy's NodePort
#   tls    : the proxy with RPC-with-TLS (xprtsec=mtls, the host's tlshd)
# Workloads: a stat pass over every file (actimeo=0: every stat is a
# GETATTR), creating + deleting small files, and a sequential O_DIRECT
# write then read. Measured: wall time, and the PROXY container's own CPU
# (cgroup usage) per 1k ops / per GiB. Same host, so this is the proxy's
# CPU and latency cost, NOT network throughput (that is the AWS session).
#   bash step6-box-proxy.sh      # REPS=3 SEQ_MIB=512; KEEP=1
source "$(cd "$(dirname "$0")" && pwd)/rig-safety.sh"
set -u
CLUSTER=flint-step6c; OUT=$HOME/nfs-proxy-step6-C; MNT=/mnt/px6d
cd "$(dirname "$0")" && source ./step6-lib.sh
REPS=${REPS:-3}; SEQ_MIB=${SEQ_MIB:-512}; FILES=5000; SMALL=2000
TLSHD_CONF=/etc/tlshd.conf
ROUTE=""
cleanup_c() {
  [ -n "$ROUTE" ] && sudo ip route del $ROUTE 2>/dev/null
  [ -f $OUT/tlshd.conf.orig ] && sudo cp $OUT/tlshd.conf.orig $TLSHD_CONF && sudo systemctl restart tlshd
  cleanup
}
trap cleanup_c EXIT INT TERM

echo "== bring-up"
up_cluster; up_operator
shares c1; wait_ready c1
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || exit 1
MOPTS=",actimeo=0" mount_proxy c1 || exit 1
seed $MNT/c1 $FILES
check "c1 holds $FILES files" '[ "$(sudo find $MNT/c1 -type f | wc -l)" = $FILES ]'
sudo umount $MNT

# The direct arm: route the worker's pod CIDR through the worker.
WIP=$(K get node $CLUSTER-worker -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
PODCIDR=$(K get node $CLUSTER-worker -o jsonpath='{.spec.podCIDR}')
ROUTE="$PODCIDR via $WIP"; sudo ip route replace $ROUTE
HUBIP=$(K -n $NS get pod $(pod c1) -o jsonpath='{.status.podIP}')
PXPOD() { K -n $OPNS get pod -l app.kubernetes.io/name=flint-lite-operator-nfs-proxy -o jsonpath='{.items[0].metadata.name}'; }
px_cpu_us() { K -n $OPNS exec $(PXPOD) -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }
hub_cpu_us() { K -n $NS exec $(pod c1) -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }

mount_arm() {  # $1 arm -> mounts $MNT and sets DIR
  local base="nfsvers=4.2,proto=tcp,hard,timeo=50,actimeo=0"
  case $1 in
    direct) sudo timeout 60 mount -t nfs4 -o $base $HUBIP:/ $MNT && DIR=$MNT ;;
    proxy)  sudo timeout 60 mount -t nfs4 -o $base,port=$PORT $PXIP:/ $MNT && DIR=$MNT/c1 ;;
    tls)    sudo timeout 60 mount -t nfs4 -o $base,port=$PORT,xprtsec=mtls $PXIP:/ $MNT && DIR=$MNT/c1 ;;
  esac
}
run_arm() {  # $1 arm, $2 rep
  local arm=$1 rep=$2
  mount_arm $arm || { echo -e "$arm\trep=$rep\tMOUNT-FAILED"; return; }
  [ "$(sudo find $DIR -maxdepth 2 -type f | head -1)" ] || { echo -e "$arm\trep=$rep\tEMPTY-DIR"; sudo umount $MNT; return; }
  local p0 h0 t0 t1 p1 h1
  # W1: stat every file, twice
  p0=$(px_cpu_us); h0=$(hub_cpu_us); t0=$(date +%s.%N)
  sudo python3 -c "
import os, sys
n = 0
for _ in range(2):
    for d, _, fs in os.walk('$DIR'):
        for f in fs:
            os.stat(os.path.join(d, f)); n += 1
print(n, file=sys.stderr)" 2>$OUT/w1.n
  t1=$(date +%s.%N); p1=$(px_cpu_us); h1=$(hub_cpu_us)
  local n1; n1=$(cat $OUT/w1.n)
  echo -e "$arm\trep=$rep\tw=stat\tops=$n1\twall_s=$(echo "$t1 - $t0" | bc)\tproxy_cpu_ms=$(( (p1 - p0) / 1000 ))\thub_cpu_ms=$(( (h1 - h0) / 1000 ))"
  # W2: create + write 4 KiB + close, then unlink, SMALL files
  p0=$(px_cpu_us); h0=$(hub_cpu_us); t0=$(date +%s.%N)
  sudo python3 -c "
import os
d = '$DIR/small-$arm-$rep'
os.makedirs(d, exist_ok=True)
for i in range($SMALL):
    with open(f'{d}/s{i}', 'wb') as f: f.write(b'x' * 4096)
for i in range($SMALL):
    os.unlink(f'{d}/s{i}')
os.rmdir(d)"
  t1=$(date +%s.%N); p1=$(px_cpu_us); h1=$(hub_cpu_us)
  echo -e "$arm\trep=$rep\tw=create+unlink\tops=$((SMALL * 2))\twall_s=$(echo "$t1 - $t0" | bc)\tproxy_cpu_ms=$(( (p1 - p0) / 1000 ))\thub_cpu_ms=$(( (h1 - h0) / 1000 ))"
  # W3: sequential O_DIRECT write, then read back and compare
  local big=$DIR/big-$arm-$rep
  sudo head -c $((SEQ_MIB * 1048576)) /dev/urandom > $OUT/big.src 2>/dev/null || head -c $((SEQ_MIB * 1048576)) /dev/urandom > $OUT/big.src
  local want; want=$(md5sum < $OUT/big.src | cut -c1-32)
  p0=$(px_cpu_us); h0=$(hub_cpu_us); t0=$(date +%s.%N)
  sudo dd if=$OUT/big.src of=$big bs=1M oflag=direct status=none
  t1=$(date +%s.%N); p1=$(px_cpu_us); h1=$(hub_cpu_us)
  echo -e "$arm\trep=$rep\tw=seq-write\tmib=$SEQ_MIB\twall_s=$(echo "$t1 - $t0" | bc)\tproxy_cpu_ms=$(( (p1 - p0) / 1000 ))\thub_cpu_ms=$(( (h1 - h0) / 1000 ))"
  p0=$(px_cpu_us); h0=$(hub_cpu_us); t0=$(date +%s.%N)
  local got; got=$(sudo dd if=$big bs=1M iflag=direct status=none | md5sum | cut -c1-32)
  t1=$(date +%s.%N); p1=$(px_cpu_us); h1=$(hub_cpu_us)
  echo -e "$arm\trep=$rep\tw=seq-read\tmib=$SEQ_MIB\twall_s=$(echo "$t1 - $t0" | bc)\tproxy_cpu_ms=$(( (p1 - p0) / 1000 ))\thub_cpu_ms=$(( (h1 - h0) / 1000 ))\tintact=$([ "$got" = "$want" ] && echo yes || echo NO)"
  sudo rm -f $big
  unmount_hard $MNT
}

echo "== plain arms, interleaved: direct, proxy"
for r in $(seq 1 $REPS); do run_arm direct $r; run_arm proxy $r; done | tee $OUT/plain.tsv

echo "== the tls arm: proxy with RPC-with-TLS, host tlshd with a client certificate"
P=$OUT/pki; mkdir -p $P
ossl() { openssl "$@" 2>/dev/null; }
ossl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 -subj "/CN=ca" -keyout $P/ca.key -out $P/ca.crt
leaf() {  # $1 name, $2 SAN
  ossl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=$1" -keyout $P/$1.key -out $P/$1.csr
  printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth,clientAuth\n' "$2" > $P/$1.ext
  ossl x509 -req -in $P/$1.csr -CA $P/ca.crt -CAkey $P/ca.key -CAcreateserial -days 2 -extfile $P/$1.ext -out $P/$1.crt
}
leaf server "IP:$PXIP"; leaf client "URI:spiffe://clusters/box"; chmod 644 $P/*
K -n $OPNS create secret tls proxy-tls --cert=$P/server.crt --key=$P/server.key >/dev/null
K -n $OPNS create configmap proxy-client-ca --from-file=ca.crt=$P/ca.crt >/dev/null
helm upgrade flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml \
  --set nfsProxy.tls.enabled=true --set nfsProxy.tls.secretName=proxy-tls \
  --set nfsProxy.tls.clientCa.configMapName=proxy-client-ca > $OUT/helm-tls.log 2>&1 || { tail $OUT/helm-tls.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null
sudo cp $TLSHD_CONF $OUT/tlshd.conf.orig
sudo tee $TLSHD_CONF >/dev/null <<Y
[debug]
loglevel=0
tls=0
nl=0
[authenticate]
[authenticate.client]
x509.truststore= $P/ca.crt
x509.certificate= $P/client.crt
x509.private_key= $P/client.key
[authenticate.server]
Y
sudo systemctl restart tlshd; sleep 2
for _ in $(seq 1 30); do [ -n "$(PXPOD)" ] && break; sleep 2; done
sleep 10
for r in $(seq 1 $REPS); do run_arm tls $r; done | tee $OUT/tls.tsv
check "the plain proxy is refused once TLS is on (a guard that the tls arm really used TLS)" \
  '! sudo timeout 30 mount -t nfs4 -o nfsvers=4.2,soft,timeo=30,retrans=1,port=$PORT $PXIP:/ $MNT 2>/dev/null || { sudo umount $MNT; false; }'

echo "== guards"
cat $OUT/plain.tsv $OUT/tls.tsv > $OUT/all.tsv
check "every arm mounted and found files" '! grep -qE "MOUNT-FAILED|EMPTY-DIR" $OUT/all.tsv'
check "every sequential read-back is intact" '! grep -q "intact=NO" $OUT/all.tsv && grep -q "intact=yes" $OUT/all.tsv'
DPX=$(awk -F'\t' '$1=="direct" {split($6,a,"="); s+=a[2]} END {print s+0}' $OUT/all.tsv)
check "the direct arm did not go through the proxy (proxy CPU over all direct runs: ${DPX} ms)" '[ "$DPX" -lt 2000 ]'

echo "== summary: per arm and workload, ranges over reps"
python3 - $OUT/all.tsv <<'P' | tee $OUT/summary.txt
import sys, collections
rows = collections.defaultdict(list)
for line in open(sys.argv[1]):
    f = line.rstrip("\n").split("\t")
    if len(f) < 6 or "=" not in f[2]: continue
    kv = dict(x.split("=", 1) for x in f[1:])
    rows[(kv["w"], f[0])].append(kv)
for (w, arm) in sorted(rows):
    rs = rows[(w, arm)]
    wall = [float(r["wall_s"]) for r in rs]
    pcpu = [int(r["proxy_cpu_ms"]) for r in rs]
    if "ops" in rs[0]:
        ops = int(rs[0]["ops"])
        rate = [ops / x for x in wall]
        per = [p / (ops / 1000) for p in pcpu]
        print(f"{w:15} {arm:6}  {min(rate):7.0f}-{max(rate):7.0f} ops/s   proxy {min(per):6.1f}-{max(per):6.1f} ms CPU per 1k ops")
    else:
        mib = int(rs[0]["mib"])
        rate = [mib / x for x in wall]
        per = [p / (mib / 1024) for p in pcpu]
        print(f"{w:15} {arm:6}  {min(rate):7.0f}-{max(rate):7.0f} MiB/s  proxy {min(per):6.0f}-{max(per):6.0f} ms CPU per GiB")
P
echo "RESULT: $ok passed, $bad failed"
