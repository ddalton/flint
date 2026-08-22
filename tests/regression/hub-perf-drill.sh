#!/usr/bin/env bash
# hub performance drill — what an agent fleet actually feels.
#
# WHY THIS SHAPE
#
# A single "MiB/s" number is nearly useless for this workload. Agents run
# git, npm, sqlite and compilers: those are dominated by METADATA round
# trips over thousands of small files, not by streaming bandwidth. So
# this measures both, and — more importantly — measures them against a
# LOCAL-DISK CONTROL in the same pod, on the same node, at the same
# moment.
#
# The control is the whole point. "NFS did 40 MiB/s" means nothing until
# you know the instance's local disk did 120 MiB/s in the same second;
# the ratio is the portable finding, the absolute number is not.
#
# ANTI-VACUITY: the fastest way to get a flattering read number is to
# measure the client's page cache. Every read here either uses O_DIRECT
# or reads bytes this client has never written (seeded through the REST
# door), so a cache hit cannot masquerade as throughput.
#
#   MODE=cluster BUCKET=... REGION=... DRILL_AK=... DRILL_SK=... \
#     ./tests/regression/hub-perf-drill.sh
set -uo pipefail

NS="${NS:-workspaces}"
OPNS="${OPNS:-flint-system}"
PROJECT="${PROJECT:-perf}"
SHARE="fs-$PROJECT"
SEQ_MB="${SEQ_MB:-512}"
SMALL_N="${SMALL_N:-2000}"
PVC_SIZE="${PVC_SIZE:-8Gi}"
PF_GW="${PF_GW:-39401}"
PF_GW_PID=""

BUCKET="${BUCKET:?needs BUCKET}"
REGION="${REGION:-us-west-1}"
HUB_AK="${DRILL_AK:?needs DRILL_AK}"
HUB_SK="${DRILL_SK:?needs DRILL_SK}"
OP_CHART="${CHART_SRC:-oci://registry-1.docker.io/dilipdalton/flint-lite-operator}"
CHART_VER="${CHART_VER:-0.2.6}"
export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-drill-helm-cache}"
mkdir -p "$HELM_CACHE_HOME" 2>/dev/null

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
# `bad` records a failed SECTION and continues. It was CALLED before it
# was ever DEFINED, so `bad: command not found` scrolled past and the run
# exited 0 with an unmeasured REST door. A drill that cannot fail is not
# a drill.
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
# STDERR on purpose: this is called from inside functions whose
# stdout is captured by command substitution (gw, derive_for). On
# stdout a single retry message becomes part of the captured value.
note() { echo "    · $*" >&2; }
s3()   { AWS_DEFAULT_REGION="$REGION" aws "$@" 2>&1; }

# EXEC INTO A READY POD, NOT `deploy/`.
#
# `kubectl exec deploy/X` picks ANY pod matching the deployment's
# selector — including one left over from a previous helm release that is
# still Terminating. Its ServiceAccount is already gone, so the API call
# it makes comes back `401 Unauthorized`, which reads like a broken token
# rather than a pod that should not have been chosen. Cost a full chain.
gw_pod() {
  kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
    --field-selector=status.phase=Running \
    -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{"\n"}{end}' \
    2>/dev/null | head -1
}
derive_for() {  # derive_for <ns/name> — retries while a rollout settles
  local ref="$1" pod out
  for _ in 1 2 3 4 5; do
    pod=$(gw_pod)
    if [ -n "$pod" ]; then
      out=$(kubectl -n "$OPNS" exec "$pod" -- \
        /usr/local/bin/flint-hub-gateway --root-key-file=/etc/flint/gateway-root/key \
        --derive-for "$ref" 2>/tmp/derive-err.txt | tr -d '\r\n')
      [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    fi
    sleep 4
  done
  return 1
}


cleanup() {
  set +e
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && { echo "KEEP=1 — objects left standing"; return; }
  # Consumers first, then claims, then volumes — a PV bound to a live
  # PVC never leaves Terminating.
  kubectl -n "$NS" delete pod perfclient --force --grace-period=0 >/dev/null 2>&1
  kubectl -n "$NS" delete flintshare "$SHARE" --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete pvc "$PROJECT-pvc" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl -n "$NS" delete pvc "$SHARE-data" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl delete pv "$PROJECT-pv" --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete secret "tok-$PROJECT" --ignore-not-found >/dev/null 2>&1
}
trap cleanup EXIT

echo "══════════════════════════════════════════════════════════════════"
echo " hub performance drill — streaming and metadata, against a control"
echo " sequential=${SEQ_MB} MiB   small files=${SMALL_N}"
echo "══════════════════════════════════════════════════════════════════"

say "preflight"
kubectl config current-context >/dev/null 2>&1 || fail "no kube context"
helm status flint-lite-operator -n "$OPNS" >/dev/null 2>&1 || fail "operator not installed — run the doc drill first, or install it"
GW_TOKEN=$(kubectl -n "$OPNS" get secret flint-gateway-token -o jsonpath='{.data.token}' 2>/dev/null | base64 -d)
[ -n "$GW_TOKEN" ] || fail "cannot read the gateway token"
EX=$(s3 s3 ls "s3://$BUCKET/$PROJECT/" --recursive 2>/dev/null | grep -c . || true)
[ "${EX:-0}" -eq 0 ] || fail "s3://$BUCKET/$PROJECT/ is not empty ($EX objects)"
pass "preflight ok"

say "creating the share"
kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: $SHARE
  namespace: $NS
  labels: { flint.io/project-id: $PROJECT }
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT/
  region: $REGION
  credentialsSecretRef: flint-s3
  persistence: { size: $PVC_SIZE }
  monitoring:
    enabled: true
    fileApi: { enabled: true, tokenSecretRef: tok-$PROJECT }
  settings:
    flushFloorSecs: 30
EOF
TOK=$(derive_for "$NS/$SHARE")
[ -n "$TOK" ] || { echo "    derive-for stderr:"; tail -3 /tmp/derive-err.txt 2>/dev/null; fail "--derive-for produced nothing"; }
kubectl -n "$NS" create secret generic "tok-$PROJECT" --from-literal=token="$TOK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
for _ in $(seq 1 60); do
  [ "$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && break
  sleep 5
done
[ "$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}')" = Ready ] \
  || { kubectl -n "$NS" get flintshare "$SHARE" -o yaml | tail -20; fail "share never Ready"; }
CIP=$(kubectl -n "$NS" get svc "$SHARE" -o jsonpath='{.spec.clusterIP}')
HUBNODE=$(kubectl -n "$NS" get pod -l flint.io/share="$SHARE" -o jsonpath='{.items[0].spec.nodeName}')
pass "share Ready on $HUBNODE, ClusterIP $CIP"

say "client pod: the NFS mount and a LOCAL DISK control, same pod"
kubectl apply -f - >/dev/null <<EOF || fail "PV/PVC refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: $PROJECT-pv }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard, nconnect=4, noatime, sec=sys]
  nfs: { server: $CIP, path: / }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: $PROJECT-pvc, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: $PROJECT-pv
  resources: { requests: { storage: 100Gi } }
EOF
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "client pod refused"
apiVersion: v1
kind: Pod
metadata: { name: perfclient, namespace: $NS }
spec:
  restartPolicy: Never
  securityContext: { runAsUser: 1000, runAsGroup: 1000, fsGroup: 1000 }
  containers:
    - name: c
      # DEBIAN, NOT ALPINE, AND THIS IS NOT A PREFERENCE.
      # BusyBox `date` has no %N, so every elapsed-time calculation in a
      # BusyBox shell evaluates to 0ms; BusyBox `dd` also prints a
      # different summary line, so the throughput parser returns nothing.
      # Run 4 of this drill "passed" reporting 0ms metadata and n/a
      # throughput. GNU coreutils is the whole fix.
      image: debian:12-slim
      command: ["sh","-c","sleep 7200"]
      volumeMounts:
        - { name: ws,    mountPath: /workspace }
        - { name: local, mountPath: /local }
  volumes:
    - name: ws
      persistentVolumeClaim: { claimName: $PROJECT-pvc }
    - name: local
      emptyDir: {}
EOF
kubectl -n "$NS" wait --for=condition=ready pod/perfclient --timeout=240s >/dev/null 2>&1 \
  || { kubectl -n "$NS" describe pod perfclient | sed -n '/Events:/,$p' | tail -10; fail "client pod never Ready"; }
cl() { kubectl -n "$NS" exec perfclient -- sh -c "$1" 2>&1; }
CLNODE=$(kubectl -n "$NS" get pod perfclient -o jsonpath='{.spec.nodeName}')
note "client on $CLNODE, hub on $HUBNODE $([ "$CLNODE" = "$HUBNODE" ] && echo '(SAME node — numbers do not cross the network)' || echo '(different nodes — real network path)')"
note "mount: $(cl 'mount | grep /workspace | sed "s/.*type //"' | head -1)"

# ── 1. sequential ────────────────────────────────────────────────────
say "1. sequential throughput, ${SEQ_MB} MiB, NFS vs local disk"
# oflag=direct bypasses the client page cache on the way out; without it
# a write returns when it hits RAM and the number is fiction.
run_seq() {  # run_seq <dir> <label>
  local d="$1" l="$2"
  local w r
  w=$(cl "dd if=/dev/zero of=$d/seq.bin bs=1M count=$SEQ_MB oflag=direct 2>&1 | tail -1" \
      | grep -oE '[0-9.]+ [KMG]B/s' | tail -1)
  [ -z "$w" ] && w=$(cl "dd if=/dev/zero of=$d/seq.bin bs=1M count=$SEQ_MB conv=fsync 2>&1 | tail -1" \
      | grep -oE '[0-9.]+ [KMG]B/s' | tail -1)
  r=$(cl "dd if=$d/seq.bin of=/dev/null bs=1M iflag=direct 2>&1 | tail -1" \
      | grep -oE '[0-9.]+ [KMG]B/s' | tail -1)
  echo "$l|${w:-n/a}|${r:-n/a}"
  cl "rm -f $d/seq.bin" >/dev/null
}
SEQ_NFS=$(run_seq /workspace NFS)
SEQ_LOC=$(run_seq /local LOCAL)
# ANTI-VACUITY. "n/a" is not a result, and a summary table full of them
# reads like a measurement. If dd's output could not be parsed, the drill
# has measured nothing and must say so.
case "$SEQ_NFS$SEQ_LOC" in
  *n/a*) bad "throughput could not be parsed from dd — no sequential number was measured"; note "raw: $SEQ_NFS / $SEQ_LOC" ;;
esac
note "$(echo "$SEQ_NFS" | awk -F'|' '{printf "%-6s write %-12s read %s", $1, $2, $3}')"
note "$(echo "$SEQ_LOC" | awk -F'|' '{printf "%-6s write %-12s read %s", $1, $2, $3}')"
pass "sequential measured (O_DIRECT where the kernel allowed it)"

# ── 2. metadata: the workload agents actually run ────────────────────
say "2. metadata: ${SMALL_N} × 4 KiB create / stat / delete, NFS vs local"
run_small() {  # run_small <dir> <label>
  local d="$1" l="$2" t0 t1 c st rm
  cl "rm -rf $d/small && mkdir -p $d/small" >/dev/null
  c=$(cl "cd $d/small && start=\$(date +%s%N) && i=0; while [ \$i -lt $SMALL_N ]; do dd if=/dev/zero of=f\$i bs=4k count=1 2>/dev/null; i=\$((i+1)); done; end=\$(date +%s%N); echo \$(( (end-start)/1000000 ))" | tail -1)
  st=$(cl "cd $d/small && start=\$(date +%s%N) && ls -l >/dev/null 2>&1 && for f in *; do stat \$f >/dev/null 2>&1; done; end=\$(date +%s%N); echo \$(( (end-start)/1000000 ))" | tail -1)
  rm=$(cl "cd $d/small && start=\$(date +%s%N) && rm -f * ; end=\$(date +%s%N); echo \$(( (end-start)/1000000 ))" | tail -1)
  cl "rm -rf $d/small" >/dev/null
  echo "$l|$c|$st|$rm"
}
SM_NFS=$(run_small /workspace NFS)
SM_LOC=$(run_small /local LOCAL)
# Same rule: a 0ms create of 2000 files did not happen.
for row in "$SM_NFS" "$SM_LOC"; do
  c=$(echo "$row" | cut -d'|' -f2)
  case "$c" in ''|0) bad "metadata timing came back '$c' for $(echo "$row"|cut -d'|' -f1) — the shell cannot measure (BusyBox date has no %N)"; ;; esac
done
fmt_small() { echo "$1" | awk -F'|' -v n="$SMALL_N" '{printf "%-6s create %6sms (%s/s)  stat %6sms  delete %6sms", $1,$2, ($2>0? int(n*1000/$2):"?"), $3, $4}'; }
note "$(fmt_small "$SM_NFS")"
note "$(fmt_small "$SM_LOC")"
pass "metadata measured"

# ── 3. the REST door ─────────────────────────────────────────────────
say "3. the REST door, ${SEQ_MB} MiB, measured FROM INSIDE THE CLUSTER"
# NOT through `kubectl port-forward`. That is a userspace relay through
# the API server: it dropped mid-transfer on two earlier runs, and any
# number taken across it is a lower bound on the gateway rather than a
# measurement of it. A pod on the cluster network is also what a real
# caller looks like.
kubectl -n "$NS" delete pod restperf --force --grace-period=0 >/dev/null 2>&1
GWURL="http://flint-lite-operator-gateway.$OPNS.svc:8090/v1/projects/$PROJECT/files/content?path=/rest.bin"
kubectl -n "$NS" run restperf --image=debian:12-slim --restart=Never --command -- \
  sh -c "apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq curl >/dev/null 2>&1;
    dd if=/dev/urandom of=/tmp/b.bin bs=1M count=$SEQ_MB 2>/dev/null;
    PC=\$(curl -s -o /dev/null -w '%{http_code} %{time_total}' -X PUT \
      -H 'Authorization: Bearer $GW_TOKEN' -H 'Content-Type: application/octet-stream' -H 'Expect:' \
      -T /tmp/b.bin '$GWURL');
    GC=\$(curl -s -o /tmp/back.bin -w '%{http_code} %{time_total}' \
      -H 'Authorization: Bearer $GW_TOKEN' '$GWURL');
    echo RESTRESULT put=\$PC get=\$GC bytes=\$(stat -c %s /tmp/back.bin 2>/dev/null)" >/dev/null 2>&1
for _ in $(seq 1 90); do
  rp=$(kubectl -n "$NS" get pod restperf -o jsonpath='{.status.phase}' 2>/dev/null)
  case "$rp" in Succeeded|Failed) break ;; esac
  sleep 5
done
RR=$(kubectl -n "$NS" logs restperf 2>/dev/null | grep -o 'RESTRESULT.*' | head -1)
kubectl -n "$NS" delete pod restperf --force --grace-period=0 >/dev/null 2>&1 &
note "in-cluster: ${RR:-<no output>}"
PUTC=$(printf '%s' "$RR" | sed -n 's/.*put=\([0-9]*\) .*/\1/p')
PUTS=$(printf '%s' "$RR" | sed -n 's/.*put=[0-9]* \([0-9.]*\).*/\1/p')
GETC=$(printf '%s' "$RR" | sed -n 's/.*get=\([0-9]*\) .*/\1/p')
GETS=$(printf '%s' "$RR" | sed -n 's/.*get=[0-9]* \([0-9.]*\).*/\1/p')
GOTB=$(printf '%s' "$RR" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p')
WANTB=$((SEQ_MB * 1024 * 1024))
REST_PUT=""; REST_GET=""
OK_PUT=0; case "${PUTC:-}" in 200|201|204) OK_PUT=1 ;; esac
if [ "$OK_PUT" = "1" ] && [ "${GOTB:-0}" = "$WANTB" ]; then
  REST_PUT=$(python3 -c "print('%.0f' % ($SEQ_MB/max(${PUTS:-0.001},0.001)))" 2>/dev/null)
  REST_GET=$(python3 -c "print('%.0f' % ($SEQ_MB/max(${GETS:-0.001},0.001)))" 2>/dev/null)
  pass "REST PUT ${SEQ_MB} MiB in ${PUTS}s (~${REST_PUT} MiB/s); GET in ${GETS}s (~${REST_GET} MiB/s); ${GOTB} bytes returned"
else
  bad "the in-cluster REST round trip did not complete (put=${PUTC:-none} get=${GETC:-none} bytes=${GOTB:-0}, wanted $WANTB) — no REST rate measured"
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " performance summary"
echo "══════════════════════════════════════════════════════════════════"
printf ' %-22s %-14s %s\n' "sequential (${SEQ_MB} MiB)" "write" "read"
echo "$SEQ_NFS" | awk -F'|' '{printf "   %-20s %-14s %s\n", $1, $2, $3}'
echo "$SEQ_LOC" | awk -F'|' '{printf "   %-20s %-14s %s\n", $1, $2, $3}'
echo
printf ' %-22s %-12s %-12s %s\n' "metadata (${SMALL_N} files)" "create" "stat" "delete"
echo "$SM_NFS" | awk -F'|' '{printf "   %-20s %-12s %-12s %s\n", $1, $2"ms", $3"ms", $4"ms"}'
echo "$SM_LOC" | awk -F'|' '{printf "   %-20s %-12s %-12s %s\n", $1, $2"ms", $3"ms", $4"ms"}'
echo
echo
printf ' %-22s %-14s %s\n' "REST door (${SEQ_MB} MiB)" "PUT" "GET"
printf '   %-20s %-14s %s\n' "in-cluster" "${REST_PUT:-?} MiB/s" "${REST_GET:-?} MiB/s"
echo
echo " client node $CLNODE / hub node $HUBNODE"
echo " The LOCAL row is the control: same pod, same node, same instant."
echo " Ratios travel between clusters; the absolute numbers do not."
echo
if [ ${#FAILURES[@]} -eq 0 ]; then
  echo "ALL SECTIONS MEASURED."
else
  echo "${#FAILURES[@]} SECTION(S) FAILED:"
  for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done
  exit 1
fi
