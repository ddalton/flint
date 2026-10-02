#!/bin/bash
# nfs-proxy step 6, phase D on the box (docs/plans/flint-lite-nfs-proxy-step6-rig-plan.md):
# the CONTROL PLANE with LIVE live stub hubs (default 500) and PARKED
# CR-only hibernated shares (default 10,000), all behind the proxy.
# Measured: operator and proxy RSS / peak / CPU settled, apiserver request
# rates by resource over a window (from the apiserver's own counters), and
# a FORCED RELIST — etcd compacted past every watcher's revision, then the
# apiserver restarted, so each watch resumes with 410 Gone and must list
# the fleet again — the spike the operator's 1Gi limit rests on
# (estimated until now, never measured).
#   bash step6-box-fleet.sh      # LIVE=500 PARKED=10000 WORKERS=6; KEEP=1
set -u
CLUSTER=flint-step6d; OUT=$HOME/nfs-proxy-step6-D; MNT=/mnt/px6e
cd "$(dirname "$0")" && source ./step6-lib.sh
LIVE=${LIVE:-500}; PARKED=${PARKED:-10000}; WORKERS=${WORKERS:-6}; BATCH=500
STUBIMG=flint-hub-stub:$TAG

echo "== images: a stub from HEAD (the published 1.32.0 predates today's status fields)"
R=$REPO/spdk-csi-driver/target/x86_64-unknown-linux-musl/release
( cd $REPO/spdk-csi-driver && cargo zigbuild --release --target x86_64-unknown-linux-musl --bin flint-hub-stub 2>&1 | grep -E "^error|Finished" )
IMG=$(mktemp -d -p $HOME); cp $R/flint-hub-stub $IMG/ || exit 1
printf 'FROM alpine:3.20\nCOPY flint-hub-stub /usr/local/bin/flint-pnfs-mds\nCOPY flint-hub-stub /usr/local/bin/flint-hub-stub\n' > $IMG/Dockerfile
docker build -q -t $STUBIMG $IMG >/dev/null || { echo "stub image build failed"; exit 1; }
rm -rf $IMG

echo "== cluster: 1 control plane + $WORKERS workers (110 pods each)"
docker image inspect $OPIMG >/dev/null 2>&1 || { echo "missing $OPIMG"; exit 1; }
kind delete cluster --name $CLUSTER >/dev/null 2>&1
{ printf 'kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\nnodes:\n  - role: control-plane\n'
  for _ in $(seq 1 $WORKERS); do printf '  - role: worker\n'; done; } > $OUT/kind.yaml
kind create cluster --name $CLUSTER --config $OUT/kind.yaml --wait 300s >/dev/null 2>&1 || { echo "kind create failed"; exit 1; }
for i in $OPIMG $STUBIMG; do kind load docker-image $i --name $CLUSTER >/dev/null 2>&1 || exit 1; done
K create ns $OPNS >/dev/null; K create ns $NS >/dev/null
K -n $NS create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=x --from-literal=AWS_SECRET_ACCESS_KEY=y >/dev/null
up_operator "resources: { requests: { cpu: 500m, memory: 128Mi }, limits: { memory: 4Gi } }
"
helm upgrade flint-lite-operator $CHART -n $OPNS -f $OUT/values.yaml \
  --set nfsProxy.resources.limits.memory=4Gi --set-string logLevel="$(echo "${OPLOG:-info}" | sed "s/,/\\\\,/g")" > $OUT/helm2.log 2>&1 || { tail $OUT/helm2.log; exit 1; }
K -n $OPNS rollout status deploy/flint-lite-operator --timeout=180s >/dev/null || exit 1
K -n $OPNS rollout status deploy/flint-lite-operator-nfs-proxy --timeout=180s >/dev/null || exit 1

mem() { K -n $OPNS exec deploy/$1 -- sh -c 'grep -E "^Vm(RSS|HWM)" /proc/1/status' 2>/dev/null | awk '{printf "%s=%dMi ", $1, $2/1024}'; }
cpu_us() { K -n $OPNS exec deploy/$1 -- sh -c 'grep usage_usec /sys/fs/cgroup/cpu.stat' 2>/dev/null | awk '{print $2}'; }
restarts() { K -n $OPNS get pod -l app.kubernetes.io/name=$1 -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}'; }
echo "empty: operator [$(mem flint-lite-operator)] proxy [$(mem flint-lite-operator-nfs-proxy)]" | tee $OUT/mem.txt

echo "== seeding $LIVE live stub shares and $PARKED parked (hibernated, CR-only) shares"
SINCE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
gen() {  # $1 from, $2 to (exclusive)
  python3 - $1 $2 $LIVE $NS $SINCE $STUBIMG <<'P'
import sys
a, b, live, ns, since, stub = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5], sys.argv[6]
for i in range(a, b):
    ann = "" if i < live else f'  annotations: {{ chert.us/idle-state: Hibernated, chert.us/idle-since: "{since}" }}\n'
    extra = f"  image: {stub}\n  resources: {{ requests: {{ cpu: 5m, memory: 16Mi }} }}\n  idle: {{}}\n" if i < live else ""
    print(f"""---
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata:
  name: p-{i:05d}
  namespace: {ns}
{ann}spec:
  bucket: fleet
  keyPrefix: p-{i:05d}/
  endpoint: http://s3.invalid:9000
  region: us-east-1
  credentialsSecretRef: s3
  persistence: {{ size: 1Gi }}
{extra}""", end="")
P
}
T0=$(date +%s); TOTAL=$((LIVE + PARKED))
for ((b = 0; b < TOTAL; b += BATCH)); do
  gen $b $(( b + BATCH < TOTAL ? b + BATCH : TOTAL )) > $OUT/batch.yaml
  K create -f $OUT/batch.yaml >/dev/null 2>>$OUT/seed.err || { echo "seeding failed at $b"; tail -3 $OUT/seed.err; exit 1; }
done
echo "seeded $TOTAL in $(( $(date +%s) - T0 ))s"
phases() { K -n $NS get flintshares -o jsonpath='{range .items[*]}{.status.phase}{"\n"}{end}' 2>/dev/null | sort | uniq -c | tr '\n' ' '; }
T1=$(date +%s); HOT=0
while [ $(( $(date +%s) - T1 )) -lt 3600 ]; do
  # Safety valves. Run 1 (500 live) took the box off the network: the
  # host's ARP table, shared by every pod's namespace, overflowed at ~350
  # pods (`arp_guard`, rig-safety.sh). It read as a load hang until the
  # journal said otherwise. The load check stays as a second guard.
  arp_guard
  L=$(cut -d' ' -f1 /proc/loadavg); if [ "${L%.*}" -ge 12 ]; then HOT=$((HOT+1)); else HOT=0; fi
  [ $HOT -ge 2 ] && { echo "ABORT: load average $L twice in a row — protecting the box"; exit 1; }
  P=$(phases); echo "  $(( $(date +%s) - T1 ))s: $P| operator [$(mem flint-lite-operator)] proxy [$(mem flint-lite-operator-nfs-proxy)]" | tee -a $OUT/mem.txt
  echo "$P" | grep -q " $LIVE Ready" && echo "$P" | grep -q " $PARKED Hibernated" && break
  for _ in 1 2 3 4 5 6; do sleep 10; arp_guard; done   # the table fills in minutes
done
SETTLE_S=$(( $(date +%s) - T1 ))
P=$(phases)
check "$LIVE live stub hubs Ready" 'echo "$P" | grep -q " $LIVE Ready"'
check "$PARKED parked shares Hibernated" 'echo "$P" | grep -q " $PARKED Hibernated"'
REACH=$(K -n $NS get flintshares -o jsonpath='{range .items[*]}{.status.conditions[?(@.type=="HubReachable")].status}{"\n"}{end}' | grep -c True)
check "the operator polls every live stub (HubReachable $REACH of $LIVE)" '[ "$REACH" -ge "$LIVE" ]'

echo "== settled: a 300 s window"
sleep 120
apireq() { K get --raw /metrics 2>/dev/null | grep '^apiserver_request_total{' | python3 -c '
import re, sys, collections
c = collections.Counter()
for line in sys.stdin:
    m = re.search(r"resource=\"([^\"]*)\"", line); v = re.search(r"verb=\"([^\"]*)\"", line)
    c[(m.group(1) if m else "", v.group(1) if v else "")] += float(line.rsplit(" ", 1)[1])
for (r, v), n in c.items(): print(f"{r}\t{v}\t{n}")'; }
apireq > $OUT/api0.tsv; o0=$(cpu_us flint-lite-operator); p0=$(cpu_us flint-lite-operator-nfs-proxy)
sleep 300
apireq > $OUT/api1.tsv; o1=$(cpu_us flint-lite-operator); p1=$(cpu_us flint-lite-operator-nfs-proxy)
echo "settled: operator [$(mem flint-lite-operator)] cpu $(( (o1 - o0) / 300000 ))m; proxy [$(mem flint-lite-operator-nfs-proxy)] cpu $(( (p1 - p0) / 300000 ))m" | tee -a $OUT/mem.txt
python3 - $OUT/api0.tsv $OUT/api1.tsv <<'P' | tee $OUT/api-rates.txt
import sys, collections
def load(p):
    d = {}
    for line in open(p):
        r, v, n = line.rstrip("\n").split("\t"); d[(r, v)] = float(n)
    return d
a, b = load(sys.argv[1]), load(sys.argv[2])
rate = {k: (b[k] - a.get(k, 0)) / 300 for k in b}
by = collections.Counter()
for (r, v), x in rate.items(): by[r] += x
print(f"apiserver requests/s over 300 s, total {sum(rate.values()):.2f}")
for r, x in by.most_common(10): print(f"  {r or '(none)':28} {x:7.2f}/s")
P

echo "== forced relist: compact etcd past every watcher, restart the apiserver"
CP=$CLUSTER-control-plane
REV=$(docker exec $CP sh -c 'crictl exec $(crictl ps --name etcd -q) etcdctl --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key endpoint status -w json' 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["Status"]["header"]["revision"])' 2>/dev/null)
echo "etcd revision $REV"
HWM0=$(mem flint-lite-operator); PHWM0=$(mem flint-lite-operator-nfs-proxy)
# Evidence that cannot be lost (run 2 at 10,000 lost both kinds):
# - VmHWM is a LIFETIME peak, and the initial sync already set it, so a
#   relist spike below it did not show. Reset it (clear_refs 5) just
#   before the restart, and read it from the node, not through the
#   apiserver that is about to go away.
# - `kubectl logs` returns only the CURRENT log file. kubelet rotates at
#   10 MiB, so at 10,000 shares a 410 line could rotate out within the
#   wait. Search every file under /var/log/pods, rotated ones included.
node_of() { K -n $OPNS get pod -l app.kubernetes.io/name=$1 -o jsonpath='{.items[0].spec.nodeName}'; }
pid_of() {  # $1 node, $2 container name: the host-side pid inside the node
  docker exec $1 sh -c "crictl inspect \$(crictl ps --name '^$2\$' -q | head -1)" | python3 -c 'import json,sys; print(json.load(sys.stdin)["info"]["pid"])'
}
ON=$(node_of flint-lite-operator); OP=$(pid_of $ON operator)
PN=$(node_of flint-lite-operator-nfs-proxy); PP=$(pid_of $PN nfs-proxy)
nodemem() { docker exec $1 sh -c "grep -E '^Vm(RSS|HWM)' /proc/$2/status" | awk '{printf "%s=%dMi ", $1, $2/1024}'; }
echo "operator: node $ON pid $OP [$(nodemem $ON $OP)]; proxy: node $PN pid $PP [$(nodemem $PN $PP)]"
oplogs() { docker exec $ON sh -c 'for f in /var/log/pods/'$OPNS'_flint-lite-operator-*/operator/*; do case $f in *.gz) zcat $f;; *) cat $f;; esac; done'; }
LOGB0=$(oplogs | wc -c)
# Advance the revision past the watchers (a no-op object churn), compact, restart the apiserver.
for i in $(seq 1 20); do K -n $NS annotate secret s3 --overwrite churn=$i >/dev/null; done
REV2=$(docker exec $CP sh -c 'crictl exec $(crictl ps --name etcd -q) etcdctl --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key endpoint status -w json' 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["Status"]["header"]["revision"])' 2>/dev/null)
docker exec $CP sh -c "crictl exec \$(crictl ps --name etcd -q) etcdctl --endpoints=https://127.0.0.1:2379 --cacert=/etc/kubernetes/pki/etcd/ca.crt --cert=/etc/kubernetes/pki/etcd/server.crt --key=/etc/kubernetes/pki/etcd/server.key compact $REV2" 2>&1 | tail -1
docker exec $ON sh -c "echo 5 > /proc/$OP/clear_refs"; docker exec $PN sh -c "echo 5 > /proc/$PP/clear_refs"
echo "peaks reset: operator [$(nodemem $ON $OP)] proxy [$(nodemem $PN $PP)]"
T_RESTART=$(date +%s); TS_RESTART=$(date -u +%Y-%m-%dT%H:%M:%S)
docker exec $CP sh -c 'crictl stop $(crictl ps --name kube-apiserver -q)' >/dev/null 2>&1
for _ in $(seq 1 60); do K get --raw /readyz >/dev/null 2>&1 && break; sleep 2; done
# kube's Controller puts ONE backoff over all its watches: each 410 after
# the restart pauses every watch for ~30-45 s, and the 410s drain one at a
# time. At 10,000 shares six of them took 3.5 min, and no re-list began
# until all had drained; the old 180 s window closed before any did.
# So watch for RELIST_WAIT (default 600 s), sampling the operator's RSS.
for _ in $(seq 1 $(( ${RELIST_WAIT:-600} / 15 ))); do
  echo "  $(date -u +%H:%M:%S) operator [$(nodemem $ON $OP)] proxy [$(nodemem $PN $PP)]" >> $OUT/relist-mem.txt
  sleep 15
done
echo "before relist: operator [$HWM0] proxy [$PHWM0]" | tee -a $OUT/mem.txt
echo "peak since the reset: operator [$(nodemem $ON $OP)] proxy [$(nodemem $PN $PP)]" | tee -a $OUT/mem.txt
LOGB1=$(oplogs | wc -c)
echo "operator log: $(( (LOGB1 - LOGB0) / 1024 )) KiB written in $(( $(date +%s) - T_RESTART ))s since the restart; files now:" | tee -a $OUT/mem.txt
docker exec $ON sh -c 'ls -la /var/log/pods/'$OPNS'_flint-lite-operator-*/operator/' | tee -a $OUT/mem.txt
# CRI lines begin with an RFC 3339 UTC timestamp (kind nodes run UTC).
# The apiserver's own words for a 410 on a watch. A bare "410" matched a
# log TIMESTAMP (13:44:27.180410437) and passed a run with no relist.
# The operator prints the error chain since 2026-10-02; before that it
# logged only "event queue error", so no pattern could have matched.
oplogs | awk -v t="$TS_RESTART" '$1 >= t' | grep -i "too old resource version" > $OUT/op-410.txt
RL=$(wc -l < $OUT/op-410.txt)
echo "operator log lines with a 410 (too old resource version) since the restart, all files: $RL" | tee -a $OUT/mem.txt
sed "s/\x1b\[[0-9;]*m//g" $OUT/op-410.txt | cut -c1-300 | head -12 | tee -a $OUT/mem.txt
oplogs | awk -v t="$TS_RESTART" '$1 >= t' | sed "s/\x1b\[[0-9;]*m//g" > $OUT/op-after-restart.log
# With OPLOG=info,kube_client=debug every request is logged with its URL:
# which watches re-opened, from which resourceVersion, and which re-LISTed.
if oplogs | awk -v t="$TS_RESTART" '$1 >= t' | grep -q 'http.url='; then
  echo "operator requests since the restart (watch/list calls only):" | tee -a $OUT/mem.txt
  oplogs | awk -v t="$TS_RESTART" '$1 >= t' | sed "s/\x1b\[[0-9;]*m//g" | grep -o 'http.method=[A-Z]* http.url=[^ ]*' \
    | python3 -c '
import sys, re, collections
seen = collections.Counter(); first = {}
for line in sys.stdin:
    m = re.search(r"http.method=(\w+) http.url=(\S+)", line)
    meth, url = m.groups()
    path, _, q = url.partition("?")
    if meth != "GET" or path.rstrip("/").split("/")[-1] in ("", "status"): continue
    res = path.rstrip("/").split("/")[-1]
    if "/namespaces/" in path and re.search(r"/namespaces/[^/]+/[^/]+/[^/]+$", path): continue  # a named GET, not a list
    kind = "WATCH" if "watch=true" in q else "LIST"
    rv = re.search(r"resourceVersion=(\d+)", q)
    key = f"{kind:5} {res}"
    seen[key] += 1
    first.setdefault(key, rv.group(1) if rv else "-")
for k in sorted(seen): print(f"  {k:40} x{seen[k]:<4} first rv={first[k]}")
' | tee -a $OUT/mem.txt
fi
check "the relist really happened (the operator logged a 410 / too-old resourceVersion)" '[ "$RL" -gt 0 ]'
check "the operator did not restart (no OOMKill)" '[ "$(restarts flint-lite-operator)" = 0 ]'
check "the proxy did not restart" '[ "$(restarts flint-lite-operator-nfs-proxy)" = 0 ]'
echo "RESULT: $ok passed, $bad failed"
