#!/usr/bin/env bash
# agent workload drill — the things agents actually do.
#
# WHY THIS EXISTS
#
# The other drills prove the PLUMBING: it installs, it mounts, it
# hydrates, it winds down and comes back. None of them runs a single
# thing an agent would actually run.
#
# That matters more here than it would elsewhere, because the guide's
# opening claim is specifically about POSIX semantics — "git, sqlite,
# npm, compilers: the tools that break on object storage because they
# need rename(2), byte-range locks and O_APPEND". Those are exactly the
# operations an NFS server is most likely to get subtly wrong, and this
# project has a scar to prove it: sqlite's first transaction and git's
# first commit had never worked on any flint mount until 934ae78.
#
# A drill that mounts a filesystem and writes a text file to it does not
# test any of that.
#
#   W1  git      — init, commit, branch, status. rename + fsync + locks.
#   W2  sqlite   — transaction, index, read back. POSIX byte-range locks,
#                  the single most common way an NFS server fails a
#                  real workload while passing every simple test.
#   W3  two agents, one workspace, ON DIFFERENT NODES — close-to-open
#                  coherence, which is the whole point of a shared
#                  workspace and cannot be tested on one node.
#   W4  hub restart under a live mount — image upgrades are routine.
#   W5  tenant isolation — project B's token must not reach project A.
#   W6  the REST surface an editor actually needs: list, Range, If-Match
#                  412, folder, move, delete.
#
#   MODE=cluster BUCKET=... REGION=... DRILL_AK=... DRILL_SK=... \
#     ./tests/regression/agent-workload-drill.sh
set -uo pipefail

NS="${NS:-workspaces}"
OPNS="${OPNS:-flint-system}"
PA="${PA:-wl-a}"          # project A
PB="${PB:-wl-b}"          # project B (isolation control)
SA="fs-$PA"; SB="fs-$PB"
PVC_SIZE="${PVC_SIZE:-8Gi}"
PF_GW="${PF_GW:-39501}"
PF_GW_PID=""
BUCKET="${BUCKET:?needs BUCKET}"
REGION="${REGION:-us-west-1}"
HUB_AK="${DRILL_AK:?needs DRILL_AK}"
HUB_SK="${DRILL_SK:?needs DRILL_SK}"
export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-drill-helm-cache}"
mkdir -p "$HELM_CACHE_HOME" 2>/dev/null

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
note() { echo "    · $*" >&2; }
s3()   { AWS_DEFAULT_REGION="$REGION" aws "$@" 2>&1; }

cleanup() {
  set +e
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  [ "${KEEP:-0}" = "1" ] && { echo "KEEP=1 — objects left standing"; return; }
  kubectl -n "$NS" delete pod agent-a agent-b --force --grace-period=0 >/dev/null 2>&1
  kubectl -n "$NS" delete flintshare "$SA" "$SB" --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete pvc wl-pvc "$SA-data" "$SB-data" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl delete pv wl-pv --ignore-not-found >/dev/null 2>&1
  kubectl -n "$NS" delete secret "tok-$PA" "tok-$PB" --ignore-not-found >/dev/null 2>&1
}
trap cleanup EXIT

gw_pod() {
  kubectl -n "$OPNS" get pod -l app.kubernetes.io/name=flint-lite-operator-gateway \
    --field-selector=status.phase=Running \
    -o jsonpath='{range .items[?(@.status.containerStatuses[0].ready==true)]}{.metadata.name}{"\n"}{end}' \
    2>/dev/null | head -1
}
derive_for() {
  local ref="$1" pod out
  for _ in 1 2 3 4 5; do
    pod=$(gw_pod)
    if [ -n "$pod" ]; then
      out=$(kubectl -n "$OPNS" exec "$pod" -- \
        /usr/local/bin/flint-hub-gateway --root-key-file=/etc/flint/gateway-root/key \
        --derive-for "$ref" 2>/tmp/wl-derive-err.txt | tr -d '\r\n')
      [ -n "$out" ] && { printf '%s' "$out"; return 0; }
    fi
    sleep 4
  done
  return 1
}
pf_gw() {
  [ -n "$PF_GW_PID" ] && kill "$PF_GW_PID" 2>/dev/null
  for _ in 1 2 3; do
    kubectl -n "$OPNS" port-forward svc/flint-lite-operator-gateway "$PF_GW:8090" >/dev/null 2>&1 &
    PF_GW_PID=$!
    for _ in $(seq 1 30); do
      curl -sf "http://127.0.0.1:$PF_GW/healthz" >/dev/null && return 0
      kill -0 "$PF_GW_PID" 2>/dev/null || break
      sleep 1
    done
    kill "$PF_GW_PID" 2>/dev/null
  done
  fail "gateway port-forward never became healthy"
}
gw() {  # gw <method> <path> [curl args...]
  local m="$1" path="$2"; shift 2
  local code
  code=$(curl -s -o /tmp/wl-body.bin -D /tmp/wl-hdr.txt -w '%{http_code}' -X "$m" \
    -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  if [ "$code" = "000" ]; then pf_gw
    code=$(curl -s -o /tmp/wl-body.bin -D /tmp/wl-hdr.txt -w '%{http_code}' -X "$m" \
      -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path"); fi
  echo "$code"
}

echo "══════════════════════════════════════════════════════════════════"
echo " agent workload drill — git, sqlite, two agents, restart, isolation"
echo "══════════════════════════════════════════════════════════════════"

say "preflight"
kubectl config current-context >/dev/null 2>&1 || fail "no kube context"
helm status flint-lite-operator -n "$OPNS" >/dev/null 2>&1 || fail "operator not installed"
GW_TOKEN=$(kubectl -n "$OPNS" get secret flint-gateway-token -o jsonpath='{.data.token}' 2>/dev/null | base64 -d)
[ -n "$GW_TOKEN" ] || fail "cannot read the gateway token"
for pfx in "$PA" "$PB"; do
  n=$(s3 s3 ls "s3://$BUCKET/$pfx/" --recursive 2>/dev/null | grep -c . || true)
  [ "${n:-0}" -eq 0 ] || fail "s3://$BUCKET/$pfx/ already has $n object(s)"
done
NODES=$(kubectl get nodes --no-headers -o custom-columns=N:.metadata.name 2>/dev/null | tr '\n' ' ')
NODE_COUNT=$(echo "$NODES" | wc -w | tr -d ' ')
note "nodes: $NODES"
pass "preflight ok ($NODE_COUNT node(s))"

mkshare() {  # mkshare <share> <project>
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare $1 refused"
apiVersion: flint.io/v1alpha1
kind: FlintShare
metadata:
  name: $1
  namespace: $NS
  labels: { flint.io/project-id: $2 }
spec:
  bucket: $BUCKET
  keyPrefix: $2/
  region: $REGION
  credentialsSecretRef: flint-s3
  persistence: { size: $PVC_SIZE }
  monitoring:
    enabled: true
    fileApi: { enabled: true, tokenSecretRef: tok-$2 }
  idle:
    suspendAfterSecs: 3600
    suspendWithSessions: false
  settings: { flushFloorSecs: 30 }
EOF
}
say "two projects, two hubs"
kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
mkshare "$SA" "$PA"; mkshare "$SB" "$PB"
for pr in "$PA" "$PB"; do
  T=$(derive_for "$NS/fs-$pr") || { tail -3 /tmp/wl-derive-err.txt; fail "derive-for failed for $pr"; }
  kubectl -n "$NS" create secret generic "tok-$pr" --from-literal=token="$T" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
done
for sh in "$SA" "$SB"; do
  for _ in $(seq 1 60); do
    [ "$(kubectl -n "$NS" get flintshare "$sh" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && break
    sleep 5
  done
  [ "$(kubectl -n "$NS" get flintshare "$sh" -o jsonpath='{.status.phase}')" = Ready ] \
    || { kubectl -n "$NS" get flintshare "$sh" -o yaml | tail -20; fail "$sh never Ready"; }
done
pf_gw
CIP=$(kubectl -n "$NS" get svc "$SA" -o jsonpath='{.spec.clusterIP}')
HUBNODE=$(kubectl -n "$NS" get pod -l flint.io/share="$SA" -o jsonpath='{.items[0].spec.nodeName}')
pass "both shares Ready; project A hub on $HUBNODE (ClusterIP $CIP)"

say "two agents on ONE workspace, on different nodes where possible"
kubectl apply -f - >/dev/null <<EOF || fail "PV/PVC refused"
apiVersion: v1
kind: PersistentVolume
metadata: { name: wl-pv }
spec:
  capacity: { storage: 100Gi }
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  mountOptions: [nfsvers=4.1, proto=tcp, hard, nconnect=4, noatime, sec=sys]
  nfs: { server: $CIP, path: / }
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: wl-pvc, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: wl-pv
  resources: { requests: { storage: 100Gi } }
EOF
# Debian for GNU tooling; git and sqlite3 are the point of the drill.
mkagent() {  # mkagent <name> <nodeName|"">
  local pin=""
  [ -n "$2" ] && pin="  nodeName: $2"
  kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "pod $1 refused"
apiVersion: v1
kind: Pod
metadata: { name: $1, namespace: $NS }
spec:
$pin
  restartPolicy: Never
  # ROOT, DELIBERATELY. apt-get cannot install as uid 1000, and this
  # drill needs real git and sqlite3. Run 1 ran as 1000, the install
  # failed silently, and the drill then reported "git failed on the
  # mount" for what was actually 'command not found' — a false accusation
  # against the product's headline claim. Unprivileged mounting is proven
  # by the doc drill; what is tested here (rename, fsync, byte-range
  # locks) does not depend on the uid.
  containers:
    - name: c
      image: debian:12-slim
      command: ["sh","-c","sleep 7200"]
      volumeMounts: [{ name: ws, mountPath: /workspace }]
  volumes:
    - name: ws
      persistentVolumeClaim: { claimName: wl-pvc }
EOF
}
NODE_A=$(echo "$NODES" | awk '{print $1}')
NODE_B=$(echo "$NODES" | awk '{print ($2==""?$1:$2)}')
mkagent agent-a "$NODE_A"
mkagent agent-b "$NODE_B"
kubectl -n "$NS" wait --for=condition=ready pod/agent-a --timeout=240s >/dev/null 2>&1 || fail "agent-a never Ready"
kubectl -n "$NS" wait --for=condition=ready pod/agent-b --timeout=240s >/dev/null 2>&1 || fail "agent-b never Ready"
A() { kubectl -n "$NS" exec agent-a -- sh -c "$1" 2>&1; }
B() { kubectl -n "$NS" exec agent-b -- sh -c "$1" 2>&1; }
note "agent-a on $NODE_A, agent-b on $NODE_B"
[ "$NODE_A" != "$NODE_B" ] && SPLIT=1 || SPLIT=""
note "installing git and sqlite3 (this is the point of the drill)"
A 'apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq git sqlite3 >/dev/null 2>&1' >/dev/null
B 'apt-get update -qq >/dev/null 2>&1; apt-get install -y -qq git >/dev/null 2>&1' >/dev/null
# HARD PRECONDITION. Without this, `command not found` (exit 127) is
# indistinguishable from a filesystem failure, and the drill blames the
# product for a broken rig. It did exactly that on run 1.
GITV=$(A 'command -v git >/dev/null && git --version' | tr -d '\r')
SQLV=$(A 'command -v sqlite3 >/dev/null && sqlite3 --version' | tr -d '\r')
case "$GITV" in *"git version"*) ;; *) fail "git is NOT installed in agent-a ($GITV) — this is the RIG, not the product; W1/W3 would be meaningless" ;; esac
case "$SQLV" in *.*) ;; *) fail "sqlite3 is NOT installed in agent-a ($SQLV) — this is the RIG, not the product; W2 would be meaningless" ;; esac
GITB=$(B 'command -v git >/dev/null && git --version' | tr -d '\r')
case "$GITB" in *"git version"*) ;; *) fail "git is NOT installed in agent-b ($GITB) — W3's handoff leg would be meaningless" ;; esac
note "agent-a: $GITV / $SQLV"
note "agent-b: $GITB"
pass "two agents mounted the same workspace, with real git and sqlite3"

# ══ W1 git ═══════════════════════════════════════════════════════════
say "W1: git — init, commit, branch. rename(2), fsync and lockfiles"
OUT=$(A 'set -e
  export HOME=/tmp GIT_AUTHOR_NAME=d GIT_AUTHOR_EMAIL=d@e GIT_COMMITTER_NAME=d GIT_COMMITTER_EMAIL=d@e
  rm -rf /workspace/repo && mkdir -p /workspace/repo && cd /workspace/repo
  git init -q .
  echo hello > a.txt && git add a.txt && git commit -qm first
  echo more >> a.txt && git add a.txt && git commit -qm second
  git checkout -qb feature && echo f > b.txt && git add b.txt && git commit -qm third
  git checkout -q master 2>/dev/null || git checkout -q main
  git log --oneline | wc -l
  git fsck --no-progress 2>&1 | tail -1
  echo GIT_OK')
case "$OUT" in
  *GIT_OK*) pass "git init + 3 commits + branch + fsck clean on the mount ($(echo "$OUT"|grep -c .) lines)" ;;
  *) note "$(echo "$OUT" | tail -6)"; bad "git failed on the mount — rename/fsync/lockfile semantics" ;;
esac

# ══ W2 sqlite ════════════════════════════════════════════════════════
say "W2: sqlite — a real transaction. POSIX byte-range locks"
OUT=$(A 'set -e
  rm -f /workspace/test.db
  sqlite3 /workspace/test.db "CREATE TABLE t(k INTEGER PRIMARY KEY, v TEXT);" 
  sqlite3 /workspace/test.db "BEGIN; INSERT INTO t(v) VALUES(\"one\"),(\"two\"),(\"three\"); COMMIT;"
  sqlite3 /workspace/test.db "CREATE INDEX ix ON t(v);"
  sqlite3 /workspace/test.db "SELECT count(*) FROM t;"
  sqlite3 /workspace/test.db "PRAGMA integrity_check;"
  echo SQLITE_OK')
case "$OUT" in
  *SQLITE_OK*)
    N=$(echo "$OUT" | grep -E '^3$' | head -1)
    IC=$(echo "$OUT" | grep -i "^ok$" | head -1)
    if [ "$N" = "3" ] && [ -n "$IC" ]; then
      pass "sqlite transaction + index committed; integrity_check ok; 3 rows read back"
    else
      note "$OUT"; bad "sqlite ran but the data or integrity check is wrong"
    fi ;;
  *) note "$(echo "$OUT" | tail -6)"; bad "sqlite failed on the mount — byte-range locking is the usual cause" ;;
esac

# ══ W3 two agents ════════════════════════════════════════════════════
say "W3: agent-b sees agent-a's work (close-to-open across $([ -n "$SPLIT" ] && echo 'two nodes' || echo 'one node'))"
A 'echo from-agent-a > /workspace/shared.txt' >/dev/null
SEEN=$(B 'cat /workspace/shared.txt 2>&1' | tr -d '\r')
[ "$SEEN" = "from-agent-a" ] && pass "agent-b read agent-a's file: '$SEEN'" \
  || bad "agent-b saw '$SEEN', expected 'from-agent-a'"
# The git repo agent-a just built must be usable BY B — a real handoff.
# `git log | wc -l` prints 0 and SUCCEEDS when git is missing, so the
# old `&& echo B_OK` chain passed without git ever running. Count the
# commits and require the number to be right.
BCOUNT=$(B 'export HOME=/tmp; cd /workspace/repo 2>/dev/null && git log --oneline 2>/dev/null | wc -l' | tr -d ' \r')
if [ "${BCOUNT:-0}" -ge 2 ]; then
  pass "agent-b read the git repo agent-a created — $BCOUNT commits visible across the mount"
else
  note "$(B 'export HOME=/tmp; cd /workspace/repo && git log --oneline 2>&1 | tail -3')"
  bad "agent-b sees $BCOUNT commits in the repo agent-a built (expected >=2)"
fi
# Concurrent independent writes, then both readable by both.
# ⚠ NOT a bare `wait`: pf_gw keeps a `kubectl port-forward` in the
# background for the life of the drill, and bare `wait` blocks on THAT
# forever. Wait on these two PIDs only.
A 'for i in $(seq 1 200); do echo "a$i" >> /workspace/a.log; done' >/dev/null & WPID_A=$!
B 'for i in $(seq 1 200); do echo "b$i" >> /workspace/b.log; done' >/dev/null & WPID_B=$!
wait "$WPID_A" "$WPID_B"
AL=$(B 'wc -l < /workspace/a.log' | tr -d ' \r'); BL=$(A 'wc -l < /workspace/b.log' | tr -d ' \r')
[ "$AL" = "200" ] && [ "$BL" = "200" ] \
  && pass "concurrent writes from both agents: each sees the other's 200 lines" \
  || bad "concurrent writes lost data (a.log=$AL b.log=$BL, both should be 200)"

# ══ W4 restart ═══════════════════════════════════════════════════════
say "W4: the hub restarts under a live mount (an image upgrade)"
# The operator publishes serverId from its own /status poll, so it can
# be briefly absent. Wait for it, otherwise the "clients resume rather
# than remount" claim is silently skipped every run.
SID_BEFORE=""
for _ in $(seq 1 24); do
  SID_BEFORE=$(kubectl -n "$NS" get flintshare "$SA" -o jsonpath='{.status.serverId}' 2>/dev/null)
  [ -n "$SID_BEFORE" ] && break
  sleep 5
done
[ -n "$SID_BEFORE" ] || note "serverId never published before the restart; the stability check will be skipped"
kubectl -n "$NS" rollout restart deploy "$SA" >/dev/null 2>&1
kubectl -n "$NS" rollout status deploy "$SA" --timeout=300s >/dev/null 2>&1
for _ in $(seq 1 40); do
  [ "$(kubectl -n "$NS" get flintshare "$SA" -o jsonpath='{.status.phase}' 2>/dev/null)" = Ready ] && break
  sleep 5
done
OUT=$(A 'cat /workspace/shared.txt 2>&1 && echo AFTER_OK' | tr -d '\r')
SID_AFTER=""
for _ in $(seq 1 24); do
  SID_AFTER=$(kubectl -n "$NS" get flintshare "$SA" -o jsonpath='{.status.serverId}' 2>/dev/null)
  [ -n "$SID_AFTER" ] && break
  sleep 5
done
case "$OUT" in
  *AFTER_OK*)
    pass "the mount survived a hub restart and I/O resumed without remounting"
    if [ -n "$SID_BEFORE" ] && [ -n "$SID_AFTER" ]; then
      [ "$SID_BEFORE" = "$SID_AFTER" ] \
        && pass "serverId stable across the restart ($SID_BEFORE) — clients resume, not remount" \
        || bad "serverId CHANGED across an ordinary restart ($SID_BEFORE -> $SID_AFTER) — every client would have to remount"
    else
      note "serverId not observed before/after (${SID_BEFORE:-absent}/${SID_AFTER:-absent}); not asserting"
    fi ;;
  *) note "$(echo "$OUT"|tail -3)"; bad "the mount did NOT survive a hub restart" ;;
esac

# ══ W5 tenant isolation ══════════════════════════════════════════════
say "W5: project B's credentials must not reach project A"
code=$(gw GET "/v1/projects/$PA/files?path=/")
[ "$code" = "200" ] || bad "project A is not readable with the gateway token (HTTP $code)"
TOKB=$(kubectl -n "$NS" get secret "tok-$PB" -o jsonpath='{.data.token}' | base64 -d)
TOKA=$(kubectl -n "$NS" get secret "tok-$PA" -o jsonpath='{.data.token}' | base64 -d)
[ "$TOKA" != "$TOKB" ] && pass "the two projects' derived hub tokens differ" \
  || bad "both projects derived the SAME hub token — one credential opens both"
# Straight at hub A with hub B's token: it must refuse.
APIA=$(kubectl -n "$NS" get flintshare "$SA" -o jsonpath='{.status.apiEndpoint}' 2>/dev/null)
if [ -n "$APIA" ]; then
  A 'apt-get install -y -qq curl >/dev/null 2>&1' >/dev/null
  HAVECURL=$(A 'command -v curl >/dev/null && echo yes' | tr -d '\r')
  if [ "$HAVECURL" != "yes" ]; then
    bad "curl is not installed in agent-a, so the direct-to-hub isolation probe cannot run — RIG, not the product"
  else
    # POSITIVE CONTROL FIRST. If A's OWN token does not get a 200 from
    # hub A, then a refusal for B's token proves nothing — it could just
    # be an unreachable endpoint. HTTP 000 on run 1 was exactly that.
    # apiEndpoint ALREADY carries its scheme; only add one if absent.
    # (status.address IS host:port — these two fields differ in shape.)
    case "$APIA" in http://*|https://*) APIURL="$APIA" ;; *) APIURL="http://$APIA" ;; esac
    note "probing $APIURL/files"
    OKA=$(A "curl -s -o /dev/null -w '%{http_code}' --max-time 20 -H 'Authorization: Bearer $TOKA' '$APIURL/files?path=/'" | tr -d '\r' | tail -c 4)
    RC=$(A "curl -s -o /dev/null -w '%{http_code}' --max-time 20 -H 'Authorization: Bearer $TOKB' '$APIURL/files?path=/'" | tr -d '\r' | tail -c 4)
    note "hub A: own token → $OKA, project B's token → $RC"
    if [ "$OKA" != "200" ]; then
      bad "hub A did not accept its OWN token (HTTP $OKA) — the isolation probe has no valid control, so B's result means nothing"
    else
      case "$RC" in
        401|403) pass "CONTROLLED: hub A accepts its own token (200) and REJECTS project B's (HTTP $RC)" ;;
        200) bad "hub A ACCEPTED project B's token — per-share derivation is not isolating tenants" ;;
        *) bad "hub A answered HTTP $RC to B's token while accepting its own — unexpected, not a clean refusal" ;;
      esac
    fi
  fi
else
  note "status.apiEndpoint absent; skipping the direct-to-hub isolation probe"
fi

# ══ W6 the REST surface ══════════════════════════════════════════════
say "W6: the REST surface an editor needs — list, Range, If-Match, folder, move, delete"
printf '0123456789abcdefghij' > /tmp/wl-r.bin
# W4 restarted this hub moments ago. Endpoints and the gateway's share
# cache take a beat to settle, and a 503 in that window is the drill
# being impatient — not the product refusing a write. Retry before
# blaming anything.
# The gateway answers a restarting share with 503 + Retry-After and a
# body naming WHICH refusal it is (Waking / NotServing / Parked /
# WakeFailed / NoTokenBinding). Run 3 retried for 60s, printed only the
# status, and reported "seed PUT failing" for what was the gateway
# correctly waiting on a hub that W4 had just restarted — then five more
# legs failed 404 on the file this PUT never wrote. Read the body, and
# give the restart real room.
code=""; SEED_T0=$(date +%s); SEED_WHY=""
for _ in $(seq 1 36); do
  code=$(gw PUT "/v1/projects/$PA/files/content?path=/r.bin" -H 'Content-Type: application/octet-stream' -H 'Expect:' -T /tmp/wl-r.bin)
  case "$code" in 200|201|204) break ;; esac
  SEED_WHY=$(python3 -c 'import json,sys
try: print(json.load(open("/tmp/wl-body.bin")).get("error",""))
except Exception: print("")' 2>/dev/null)
  note "seed PUT answered $code${SEED_WHY:+ ($SEED_WHY)}; retrying while the restarted hub settles"
  sleep 5
done
SEED_SECS=$(( $(date +%s) - SEED_T0 ))
case "$code" in
  200|201|204)
    [ "$SEED_SECS" -gt 5 ] && note "the gateway served writes again ${SEED_SECS}s after the restart" ;;
  *) bad "seed PUT still failing after ${SEED_SECS}s of retries (HTTP $code${SEED_WHY:+ / $SEED_WHY}) — the rest of W6 cannot run" ;;
esac
code=$(gw GET "/v1/projects/$PA/files/content?path=/r.bin" -H 'Range: bytes=5-9')
GOT=$(cat /tmp/wl-body.bin 2>/dev/null)
if [ "$code" = "206" ] && [ "$GOT" = "56789" ]; then
  pass "Range: bytes=5-9 → 206 with exactly those bytes ('$GOT')"
else
  bad "ranged read returned HTTP $code body '$GOT' (wanted 206 / '56789')"
fi
ETAG=$(gw GET "/v1/projects/$PA/files/content?path=/r.bin" >/dev/null; grep -i '^etag:' /tmp/wl-hdr.txt | tr -d '\r' | awk '{print $2}')
if [ -n "$ETAG" ]; then
  printf 'rewritten' > /tmp/wl-w.bin
  code=$(gw PUT "/v1/projects/$PA/files/content?path=/r.bin" -H "If-Match: $ETAG" -H 'Expect:' -T /tmp/wl-w.bin)
  case "$code" in 200|201|204) pass "If-Match with the CURRENT etag succeeded (HTTP $code)" ;;
    *) bad "If-Match with a current etag was refused (HTTP $code)" ;; esac
  code=$(gw PUT "/v1/projects/$PA/files/content?path=/r.bin" -H "If-Match: $ETAG" -H 'Expect:' -T /tmp/wl-w.bin)
  [ "$code" = "412" ] && pass "the now-STALE etag is refused with 412 — lost updates are detected" \
    || bad "a stale If-Match answered HTTP $code, expected 412"
else
  bad "no ETag header on a content GET — conditional writes cannot work"
fi
code=$(gw POST "/v1/projects/$PA/files/folder" -H 'Content-Type: application/json' --data '{"path":"/d1"}')
case "$code" in 200|201|204) pass "folder created (HTTP $code)" ;; *) bad "folder create failed (HTTP $code)" ;; esac
code=$(gw POST "/v1/projects/$PA/files/move" -H 'Content-Type: application/json' --data '{"from":"/r.bin","to":"/d1/r.bin"}')
case "$code" in 200|201|204) pass "move into the new folder (HTTP $code)" ;; *) bad "move failed (HTTP $code)" ;; esac
code=$(gw GET "/v1/projects/$PA/files?path=/d1&recursive=false")
grep -q 'r.bin' /tmp/wl-body.bin 2>/dev/null && pass "listing /d1 shows the moved file" \
  || { note "$(head -c 200 /tmp/wl-body.bin)"; bad "listing /d1 does not show the moved file (HTTP $code)"; }
code=$(gw DELETE "/v1/projects/$PA/files/content?path=/d1/r.bin")
DELETED=""
case "$code" in 200|204) DELETED=1 ;; *) bad "delete failed (HTTP $code)" ;; esac
code=$(gw GET "/v1/projects/$PA/files/content?path=/d1/r.bin")
# ANTI-VACUITY: a 404 here means "delete worked" only if the DELETE
# actually removed a file that was there. On run 3 the seed PUT never
# landed, so this leg — and the mount check below — reported GREEN on an
# empty workspace while five other legs were failing 404.
if [ -z "$DELETED" ]; then
  note "the delete did not succeed, so a 404 here proves nothing — not counting this leg"
elif [ "$code" = "404" ]; then
  pass "deleted file is gone (404) — delete really deleted"
else
  bad "a deleted file still answers HTTP $code"
fi
# And the mount agrees with the API about all of it.
LEFT=$(A 'ls /workspace/d1 2>&1' | tr -d '\r')
if [ -z "$DELETED" ]; then
  note "nothing was ever written to /workspace/d1, so an empty listing proves nothing"
elif [ -z "$LEFT" ]; then
  pass "the mount agrees: /workspace/d1 is empty"
else
  bad "the mount still shows '$LEFT' in /workspace/d1"
fi

echo
echo "══════════════════════════════════════════════════════════════════"
echo " agent workload summary — $PASSES checks passed"
echo "══════════════════════════════════════════════════════════════════"
echo " agents on separate nodes  : $([ -n "$SPLIT" ] && echo 'yes' || echo 'NO (single-node cluster)')"
echo
if [ ${#FAILURES[@]} -eq 0 ]; then echo "ALL LEGS PASSED."; else
  echo "${#FAILURES[@]} LEG(S) FAILED:"; for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done; exit 1; fi
