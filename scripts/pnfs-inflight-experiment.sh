#!/usr/bin/env bash
# WHY IS A pNFS READ 350 MiB/s WHEN THE SAME PATH ONCE DID 2432?
#
#   ./scripts/pnfs-inflight-experiment.sh <client-node> <ds-node> [pvc]
#
# THE QUESTION. On runaz, pNFS reads came in two modes — ~350 MiB/s (23 of
# 23 reads bar five) and ~2450 MiB/s — on ONE unchanging volume, warm
# mount, same pod. I claimed the slow mode was "TCP loss on the client
# path". That was refuted: the counter I sampled (/proc/net/snmp Tcp
# RetransSegs) counts segments THIS HOST retransmitted, and during a READ
# the client is the receiver — it sends only requests and pure ACKs, which
# are never retransmitted. The DS's counter, the one that would evidence
# loss in the data direction, was never sampled. The claimed inverse
# correlation is Spearman +0.000 and is POSITIVE across the four slow reads.
#
# THE REFRAME. ~350-400 MiB/s is the STEADY STATE and 2432 is the anomaly.
# `pnfs/ds/io.rs:251-253` already recorded 396 MiB/s pNFS vs 845 MiB/s
# local block on runaw, a different cluster, weeks earlier.
#
# THE DISCRIMINATOR. Throughput is FLAT from width 1 to width 5. Five data
# servers are five processes on five nodes with five NICs and no shared
# state, so any SERVER-side limit must be ADDITIVE. It isn't. That points
# at the shared CLIENT side: readahead, in-flight RPC depth, or per-stream
# sequentiality through an 8 MiB stripe unit. Two numbers decide it, and
# both come from mountstats:
#
#     average RPC size  = Δbytes_recv / Δops
#     average in-flight = Δexecute_ms / wall_ms      (Little's Law)
#
#   ~131072 B and/or 1-2 in flight  -> C1, the client is starved. A mount
#                                      tunable, not a flint defect.
#   ~1 MiB and >=8 in flight, time in RTT not queue, with DS-side cwnd
#   depressed and DS RetransSegs/OutSegs elevated -> C2/C4, the wire.
#
# Width 1 produced BOTH modes on runaz, so this needs ONE data server and
# ONE client — three nodes, not seven.
#
# EVERY INSTRUMENT HERE HAD TO EARN ITS PLACE. The previous harness
# produced three false readings in one session: a CPU meter that read 0.00
# for every point including its own idle floor (and thereby SATISFIED the
# script's decision rule), a loss counter on the wrong host in the wrong
# direction with no denominator, and a "METERED — this is AWS, not flint"
# banner asserting a cause that anti-correlated with slowness. So: hard
# preconditions that abort, both directions sampled, denominators always
# captured, no editorialising, and a clock on every line.
set -uo pipefail
CLIENT=${1:?usage: pnfs-inflight-experiment.sh <client-node> <ds-node> [pvc]}
DSNODE=${2:?}
PVC=${3:-inflight}
NS=${FLINT_NS:-flint-system}
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${OUT_DIR:-/tmp/pnfs-inflight}
SHARDS=${SHARDS:-4}
SHARD_GIB=${SHARD_GIB:-2}
WORKERS=${WORKERS:-4}
N_STOCK=${N_STOCK:-10}
N_TUNED=${N_TUNED:-5}
RA_KB=${RA_KB:-15360}
mkdir -p "$OUT"
: "${KUBECONFIG:?set KUBECONFIG}"

ts() { date +%H:%M:%S; }
say() { printf "[%s] %s\n" "$(ts)" "$*"; }
die() { printf "[%s] ✗ ABORT: %s\n" "$(ts)" "$*"; exit 1; }

say "pNFS in-flight experiment — client=$CLIENT ds=$DSNODE pvc=$PVC"

# ── PRECONDITIONS. Each one is here because its absence silently ────────
# ── invalidated an earlier run. ─────────────────────────────────────────
DS_PODS=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{range .items[*]}{.spec.nodeName}{"\n"}{end}' 2>/dev/null | sort -u)
DS_COUNT=$(echo "$DS_PODS" | grep -c . || true)
[ "$DS_COUNT" = "1" ] || die "expected exactly 1 data server, found $DS_COUNT ($DS_PODS).
    Width-1 behaviour is the whole point; more DSes reintroduce the fan-out
    variable this experiment exists to remove."
echo "$DS_PODS" | grep -qx "$DSNODE" || die "the DS is on '$DS_PODS', not '$DSNODE'"
echo "$DS_PODS" | grep -qx "$CLIENT" && die "client $CLIENT also hosts the DS — a loopback read measures a different path"
say "✓ exactly one data server, on $DSNODE; client $CLIENT is DS-free"

# The DS process, for its own read_bytes and TCP counters.
DS_PID=$("$HERE/nodesh.sh" "$DSNODE" 'pgrep -f flint-pnfs-ds | head -1' 2>/dev/null | tail -1)
[ -n "${DS_PID:-}" ] || die "no flint-pnfs-ds process found on $DSNODE"
say "✓ DS pid $DS_PID"

# drop_caches must be PROVEN to work, not assumed. Verified by watching
# the cache actually fall.
drop_client_cache() {
  local before after
  before=$("$HERE/nodesh.sh" "$CLIENT" 'free -m | awk "/Mem:/{print \$6}"' 2>/dev/null | tail -1)
  "$HERE/nodesh.sh" "$CLIENT" 'sync; echo 3 > /proc/sys/vm/drop_caches' >/dev/null 2>&1
  after=$("$HERE/nodesh.sh" "$CLIENT" 'free -m | awk "/Mem:/{print \$6}"' 2>/dev/null | tail -1)
  [ -n "${before:-}" ] && [ -n "${after:-}" ] || die "cannot read client cache size"
  echo "${before:-0} ${after:-0}"
}

# ── SAMPLERS ────────────────────────────────────────────────────────────
# Client mountstats for the ONE nfs4 mount. Asserting exactly one mount is
# what makes "the READ counters" unambiguous — and it enforces the
# one-mount-per-client rule that the pNFS nconnect caveat requires.
# The per-op line is TAB + EIGHT SPACES + "READ:", so an anchored /^\tREAD:/
# never matches and every field silently reads 0 — which is exactly what the
# first runba attempt printed (RPC 0.0 KiB, inflight 0.00) while happily
# reporting throughput beside it. Match the token, not the indentation.
client_mountstats() {
  "$HERE/nodesh.sh" "$CLIENT" '
    awk "/^device .* fstype nfs4? /{m++} END{print \"MOUNTS \" m+0}" /proc/1/mountstats
    awk "/[[:space:]]READ:/{print \"READ\", \$2, \$6, \$7, \$8, \$9; exit}" /proc/1/mountstats' \
    2>/dev/null
}
# ops=$2  bytes_recv=$6  queue_ms=$7  rtt_ms=$8  execute_ms=$9

client_tcpext() {
  "$HERE/nodesh.sh" "$CLIENT" '
    nstat -az 2>/dev/null | awk "
      /^TcpExtTCPOFOQueue/{a=\$2} /^TcpExtTCPDSACKRecv/{b=\$2}
      /^TcpExtTCPSACKReorder/{c=\$2} /^TcpExtPruneCalled/{d=\$2}
      /^TcpRetransSegs/{e=\$2} /^TcpOutSegs/{f=\$2} /^TcpInSegs/{g=\$2}
      END{print (a+0),(b+0),(c+0),(d+0),(e+0),(f+0),(g+0)}"' 2>/dev/null | tail -1
}

# THE COUNTER THAT WAS MISSING LAST TIME: the DS is the SENDER, so loss in
# the data direction lands in ITS RetransSegs. OutSegs is captured with it
# so a RATE can be computed — a bare retransmit count has no denominator.
# MUST BE READ INSIDE THE DS POD'S NETWORK NAMESPACE. flint-pnfs-ds does
# NOT set hostNetwork, so its sockets live in the pod netns and the host's
# /proc/net/snmp cannot see them. Measured on runba at the same instant:
#   host netns  OutSegs=419,231    RetransSegs=1
#   pod  netns  OutSegs=3,983,001  RetransSegs=1,757
# The first runba attempt sampled the host and reported "0/495" for a read
# that moved 8 GiB — off by four orders of magnitude, and pointing at the
# wrong conclusion (no loss) with total confidence.
DS_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
  -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
ds_tcp() {
  kubectl exec -n "$NS" "$DS_POD" -- awk \
    '/^Tcp:/{n++; if(n==2) print $13, $12}' /proc/net/snmp 2>/dev/null | tail -1
}

# Settles the DS-page-cache candidate for free: read_bytes counts bytes
# this process actually pulled from the BLOCK LAYER, so ~0 means the read
# was served from page cache. rchar is the syscall-level total and is
# captured beside it as the denominator — on runba read_bytes was 0 while
# rchar was 34 GiB, i.e. every byte came from cache and none from disk.
ds_io() {
  "$HERE/nodesh.sh" "$DSNODE" \
    "awk '/^read_bytes:/{print \$2}' /proc/$DS_PID/io" 2>/dev/null | tail -1
}

# Also inside the pod netns, for the same reason as ds_tcp.
ds_sockets() {
  kubectl exec -n "$NS" "$DS_POD" -- sh -c \
    'ss -tim 2>/dev/null | grep -A1 ":2049" | head -20' 2>/dev/null | tr '\n' ' '
}

# ── the consumer pod: ONE, kept warm for the whole experiment ───────────
POD="inflight"
kubectl delete pod "$POD" --ignore-not-found --wait=true >/dev/null 2>&1
kubectl create configmap cm-inflight --from-file=bench.py="$HERE/pnfs-model-bench.py" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl get pvc "$PVC" >/dev/null 2>&1 || cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: $PVC}
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: ${SC:-flint-pnfs}
  resources: {requests: {storage: 64Gi}}
YAML
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $POD}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: $CLIENT}
  containers:
  - name: b
    image: python:3.12-alpine
    command: ["sh","-c","sleep 100000"]
    volumeMounts: [{name: d, mountPath: /data}, {name: s, mountPath: /bench}]
  volumes:
  - name: d
    persistentVolumeClaim: {claimName: $PVC}
  - name: s
    configMap: {name: cm-inflight}
YAML
kubectl wait --for=condition=Ready "pod/$POD" --timeout=300s >/dev/null 2>&1 \
  || die "consumer pod not Ready"
say "✓ consumer pod ready on $CLIENT"

MOUNTS=$(client_mountstats | awk '/^MOUNTS/{print $2}')
[ "${MOUNTS:-0}" = "1" ] || die "client has ${MOUNTS:-0} nfs4 mounts, need exactly 1.
    Every pNFS PVC on a node shares one nfs_client and its nconnect pool,
    so a second mount makes the READ counters ambiguous."
say "✓ exactly one nfs4 mount on the client"

# lay the checkpoint down once, outside every measured window
if ! kubectl exec "$POD" -- sh -c 'ls /data/model-*.safetensors >/dev/null 2>&1'; then
  say "laying out ${SHARDS}x${SHARD_GIB}GiB checkpoint (once, outside the window)"
  kubectl exec "$POD" -- python3 /bench/bench.py write --dir /data \
    --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream --workers "$WORKERS" \
    2>&1 | grep RESULT || die "layout write failed"
fi

# ── one measured read, fully bracketed ──────────────────────────────────
: > "$OUT/points.tsv"
printf "arm\tn\tmibps\trpc_bytes\tinflight\tqueue_ms\trtt_ms\tds_read_MiB\tds_retrans\tds_outsegs\tcli_ofo\tcli_dsack\tcli_reorder\tcli_prune\tepoch\n" \
  >>"$OUT/points.tsv"

one_read() {  # arm, n
  local arm=$1 n=$2
  local cache ms0 ms1 tx0 tx1 dio0 dio1 dt0 dt1 t0 t1 res mibps
  cache=$(drop_client_cache)
  ms0=$(client_mountstats); tx0=$(client_tcpext); dio0=$(ds_io); dt0=$(ds_tcp)
  t0=$(date +%s%N)
  res=$(kubectl exec "$POD" -- python3 /bench/bench.py read --dir /data \
        --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream \
        --workers "$WORKERS" 2>&1 | grep RESULT)
  t1=$(date +%s%N)
  ms1=$(client_mountstats); tx1=$(client_tcpext); dio1=$(ds_io); dt1=$(ds_tcp)
  mibps=$(sed -n 's/.*mibps=\([0-9]*\).*/\1/p' <<<"$res")
  [ -n "${mibps:-}" ] || die "read $arm/$n produced no result — refusing to record a '?' point"

  python3 - "$arm" "$n" "$mibps" "$t0" "$t1" "$ms0" "$ms1" "$tx0" "$tx1" \
             "$dio0" "$dio1" "$dt0" "$dt1" "$OUT/points.tsv" <<'PY'
import sys
arm,n,mibps,t0,t1 = sys.argv[1],sys.argv[2],int(sys.argv[3]),int(sys.argv[4]),int(sys.argv[5])
def rd(blob):
    for ln in blob.splitlines():
        f=ln.split()
        if f and f[0]=='READ':
            return [int(x) for x in f[1:6]]      # ops bytes queue rtt execute
    return [0,0,0,0,0]
a,b = rd(sys.argv[6]), rd(sys.argv[7])
d=[y-x for x,y in zip(a,b)]
ops,byts,queue,rtt,execu = d
wall_ms=(t1-t0)/1e6
rpc_bytes = byts/ops if ops else 0
inflight  = execu/wall_ms if wall_ms else 0
q_per     = queue/ops if ops else 0
r_per     = rtt/ops if ops else 0
tx0=[int(x) for x in sys.argv[8].split()] if sys.argv[8].strip() else [0]*7
tx1=[int(x) for x in sys.argv[9].split()] if sys.argv[9].strip() else [0]*7
ofo,dsack,reorder,prune = [y-x for x,y in zip(tx0[:4],tx1[:4])]
dsread=(int(sys.argv[11] or 0)-int(sys.argv[10] or 0))/1048576
dt0=[int(x) for x in sys.argv[12].split()] if sys.argv[12].strip() else [0,0]
dt1=[int(x) for x in sys.argv[13].split()] if sys.argv[13].strip() else [0,0]
dsretr,dsout = [y-x for x,y in zip(dt0,dt1)]
with open(sys.argv[14],'a') as fh:
    fh.write(f"{arm}\t{n}\t{mibps}\t{rpc_bytes:.0f}\t{inflight:.2f}\t{q_per:.3f}\t{r_per:.3f}"
             f"\t{dsread:.0f}\t{dsretr}\t{dsout}\t{ofo}\t{dsack}\t{reorder}\t{prune}\t{t0//10**9}\n")
loss = (dsretr/dsout*100) if dsout else 0
print(f"  {arm:<6} #{n:<3} {mibps:>5} MiB/s | RPC {rpc_bytes/1024:>7.1f} KiB | inflight {inflight:>5.2f} "
      f"| q {q_per:>6.3f}ms rtt {r_per:>6.3f}ms | DS disk {dsread:>6.0f} MiB "
      f"| DS retrans {dsretr}/{dsout} ({loss:.3f}%) | cli ofo {ofo} dsack {dsack}")
PY
}

# ── INSTRUMENT SELF-TEST. Every meter must move before any of them is ──
# ── believed. Three separate instruments have now failed by reading ────
# ── ZERO rather than erroring — a CPU meter whose 0.00 satisfied the ───
# ── script's own decision rule, a mountstats regex that missed the ─────
# ── indentation, and TCP counters read in the wrong netns. In each case
# ── the run continued and produced a confident table. Not again.
say "── instrument self-test (one throwaway read) ──"
_ms0=$(client_mountstats); _dt0=$(ds_tcp); _di0=$(ds_io)
drop_client_cache >/dev/null
kubectl exec "$POD" -- python3 /bench/bench.py read --dir /data \
  --shards "$SHARDS" --shard-gib "$SHARD_GIB" --mode stream --workers "$WORKERS" \
  >/dev/null 2>&1 || die "self-test read failed"
_ms1=$(client_mountstats); _dt1=$(ds_tcp); _di1=$(ds_io)
_ops=$(python3 -c "
def rd(b):
    for l in b.splitlines():
        f=l.split()
        if f and f[0]=='READ': return int(f[1])
    return 0
print(rd('''$_ms1''') - rd('''$_ms0'''))")
_out=$(python3 -c "
a='''$_dt0'''.split(); b='''$_dt1'''.split()
print(int(b[1])-int(a[1]) if len(a)>1 and len(b)>1 else 0)")
EXPECT_OPS=$((SHARDS * SHARD_GIB * 1024 / 2))   # ~1 MiB RPCs, allow 2x slack
say "  mountstats READ ops delta = $_ops (expect >$EXPECT_OPS for $((SHARDS*SHARD_GIB)) GiB)"
say "  DS OutSegs delta          = $_out (expect >100000)"
[ "${_ops:-0}" -gt "$EXPECT_OPS" ] || die "mountstats READ counter is not moving (got $_ops).
    The per-op line is TAB + spaces + 'READ:' — an anchored regex silently
    yields 0 for every field while throughput still prints beside it."
[ "${_out:-0}" -gt 100000 ] || die "DS OutSegs is not moving (got $_out).
    flint-pnfs-ds is NOT hostNetwork, so its counters are in the POD netns;
    the host's /proc/net/snmp cannot see them and reads ~500 instead of ~4M."
say "  ✓ all meters move — proceeding"

say "── arm 1: stock settings, $N_STOCK reads ──"
for i in $(seq 1 "$N_STOCK"); do one_read stock "$i"; done

# The intervention. If C1 is right this moves the number; if the wire is
# the constraint it will not.
say "── setting NFS bdi read_ahead_kb=$RA_KB on the client ──"
"$HERE/nodesh.sh" "$CLIENT" "
  for b in /sys/class/bdi/*; do
    case \$(basename \$b) in 0:*) echo $RA_KB > \$b/read_ahead_kb 2>/dev/null;; esac
  done
  grep -H . /sys/class/bdi/0:*/read_ahead_kb 2>/dev/null | head -3" 2>/dev/null | tail -3

say "── arm 2: read_ahead_kb=$RA_KB, $N_TUNED reads ──"
for i in $(seq 1 "$N_TUNED"); do one_read tuned "$i"; done

# ── verdict ─────────────────────────────────────────────────────────────
echo
python3 - "$OUT/points.tsv" <<'PY'
import sys, statistics as st
rows=[l.split('\t') for l in open(sys.argv[1]).read().splitlines()[1:] if l.strip()]
if not rows: print("no points"); sys.exit(0)
def col(r,i,f=float): return f(r[i])
stock=[r for r in rows if r[0]=='stock']; tuned=[r for r in rows if r[0]=='tuned']
def summarise(rs,label):
    if not rs: return None
    mb=[col(r,2) for r in rs]; rpc=[col(r,3) for r in rs]; inf=[col(r,4) for r in rs]
    dsr=[col(r,7) for r in rs]; ret=[col(r,8) for r in rs]; out=[col(r,9) for r in rs]
    print(f"{label:<7} n={len(rs):<3} MiB/s {min(mb):.0f}-{max(mb):.0f} (med {st.median(mb):.0f}) "
          f"| RPC {st.median(rpc)/1024:.1f} KiB | inflight {st.median(inf):.2f} "
          f"| DS disk {st.median(dsr):.0f} MiB | DS loss {sum(ret)/sum(out)*100 if sum(out) else 0:.3f}%")
    return dict(mb=mb,rpc=st.median(rpc),inf=st.median(inf),dsr=st.median(dsr),
                loss=(sum(ret)/sum(out)*100 if sum(out) else 0))
s=summarise(stock,'stock'); t=summarise(tuned,'tuned')
print()
if not s: sys.exit(0)
fast=[m for m in s['mb'] if m>1500]; slow=[m for m in s['mb'] if m<=1500]
print(f"modes in stock arm: {len(fast)} fast, {len(slow)} slow")
print()
print("DISCRIMINATION")
if s['rpc'] < 300*1024 or s['inf'] < 4:
    print(f"  -> C1 (CLIENT STARVED). RPC {s['rpc']/1024:.1f} KiB, in-flight {s['inf']:.2f}.")
    print("     The client is not keeping enough read data outstanding. This is a")
    print("     mount/readahead tunable, NOT a flint defect.")
else:
    print(f"  -> NOT C1. RPC {s['rpc']/1024:.1f} KiB with {s['inf']:.2f} in flight is a")
    print("     well-fed client, so the constraint is downstream of it.")
if s['dsr'] < 100:
    print(f"  -> C3 DEAD. DS pulled {s['dsr']:.0f} MiB from disk; reads are DS page-cache hits.")
elif slow and fast:
    print(f"  -> C3 LIVE: DS disk reads {s['dsr']:.0f} MiB — compare fast vs slow rows above.")
if s['loss'] > 0.01:
    print(f"  -> C2/C4 LIVE. DS-side retransmission {s['loss']:.3f}% — loss IS present in the")
    print("     data direction (the thing never measured on runaz).")
else:
    print(f"  -> C2/C4 WEAKENED. DS-side retransmission {s['loss']:.3f}% is negligible;")
    print("     loss in the data direction is not the story.")
if t:
    ratio=st.median(t['mb'])/st.median(s['mb']) if st.median(s['mb']) else 0
    print(f"  -> readahead intervention: {ratio:.2f}x "
          f"({'CONFIRMS C1' if ratio>1.3 else 'does NOT move it — C1 weakened'})")
PY

echo
say "raw: $OUT/points.tsv"
kubectl delete pod "$POD" --ignore-not-found --wait=false >/dev/null 2>&1
kubectl delete configmap cm-inflight --ignore-not-found >/dev/null 2>&1
