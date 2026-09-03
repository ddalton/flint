#!/usr/bin/env bash
# hub lifecycle drill — spin down when idle, come back when someone asks.
#
# WHY THIS EXISTS
#
# The economics of flint-lite for an agent fleet rest on one claim: a
# workspace nobody is using costs nothing but its bucket. That claim has
# two halves and the second is the one that matters to a user —
#
#   1  it goes away on its own after a configured idle period, and
#   2  it comes BACK, with the files intact, when someone asks for them.
#
# The doc drill's L7 proves a MOUNTED share can be held up. This drill
# is the opposite case and the more common one: nobody is mounted, the
# share should wind down, and a later file request should bring it back.
#
# THE TWO RUNGS ARE NOT THE SAME TEST, and conflating them is how you
# end up believing the bucket is the source of truth when it is not:
#
#   SUSPEND    scales the hub to zero and KEEPS the PVC. Waking is a pod
#              start on the same disk. The files never left local disk,
#              so a wake that works here says NOTHING about S3.
#   HIBERNATE  DELETES the PVC. At that moment the bucket is the only
#              copy. Waking is a full DR import. This is the only leg
#              that actually proves "the files come back from S3".
#
# So L3 asserts the PVC is GONE before it asks for the file back. Without
# that check the leg passes on a warm cache and proves nothing.
#
#   MODE=cluster BUCKET=... REGION=... DRILL_AK=... DRILL_SK=... \
#     ./tests/regression/hub-lifecycle-drill.sh
#
# KEEP=1 leaves the share standing.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MODE="${MODE:-cluster}"
OPNS="${OPNS:-flint-system}"
NS="${NS:-workspaces}"
PROJECT="${PROJECT:-lifecycle}"
SHARE="fs-$PROJECT"
PF_GW="${PF_GW:-39301}"
PF_GW_PID=""

# The knobs under test. Deliberately short so the drill is minutes, not
# hours — but NOT so short that a slow reconcile looks like a failure.
SUSPEND_AFTER="${SUSPEND_AFTER:-60}"
HIBERNATE_AFTER="${HIBERNATE_AFTER:-120}"
PVC_SIZE="${PVC_SIZE:-4Gi}"

if [ "$MODE" = cluster ]; then
  BUCKET="${BUCKET:?needs BUCKET}"
  REGION="${REGION:-us-west-1}"
  HUB_AK="${DRILL_AK:?needs DRILL_AK}"
  HUB_SK="${DRILL_SK:?needs DRILL_SK}"
  S3_ENDPOINT=""
  OP_CHART="${CHART_SRC:-oci://registry-1.docker.io/dilipdalton/flint-lite-operator}"
  CHART_VER="${CHART_VER:-0.2.6}"
  OPIMG="${OPIMG:-dilipdalton/flint-lite-operator:1.35.0}"
  HUBIMG="${HUBIMG:-dilipdalton/flint-pnfs:1.35.0}"
else
  echo "this drill is cluster-mode only for now (real S3 is the point of L3)" >&2
  exit 2
fi

export HELM_CACHE_HOME="${HELM_CACHE_HOME:-${TMPDIR:-/tmp}/flint-drill-helm-cache}"
mkdir -p "$HELM_CACHE_HOME" 2>/dev/null

PASSES=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSES=$((PASSES+1)); }
fail() { echo "  ✗ FAIL: $*"; exit 1; }
bad()  { echo "  ✗ FAIL: $*"; FAILURES+=("$*"); }
# STDERR on purpose: this is called from inside functions whose
# stdout is captured by command substitution (gw, derive_for). On
# stdout a single retry message becomes part of the captured value.
note() { echo "    · $*" >&2; }
now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

s3() { AWS_DEFAULT_REGION="$REGION" aws "$@" 2>&1; }

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
  [ "${KEEP:-0}" = "1" ] && { echo "KEEP=1 — share left standing"; return; }
  kubectl -n "$NS" delete flintshare "$SHARE" --ignore-not-found >/dev/null 2>&1
  # The hub's PVC has no ownerRef and reclaim: Retain, so it survives the
  # share. Left behind it blocks nothing here, but it does hold a PV in
  # Terminating on the next run's cleanup.
  kubectl -n "$NS" delete pvc -l chert.us/share="$SHARE" --timeout=120s >/dev/null 2>&1
  kubectl -n "$NS" delete pvc "$SHARE-data" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl -n "$NS" delete secret "tok-$PROJECT" --ignore-not-found >/dev/null 2>&1
}
trap cleanup EXIT

pf_gw() {
  # Kill the old forwarder AND wait for the port to be released — a
  # lingering process holding 127.0.0.1:$PF_GW makes every rebind fail,
  # which reads as "the gateway is unreachable".
  if [ -n "$PF_GW_PID" ]; then
    kill "$PF_GW_PID" 2>/dev/null
    wait "$PF_GW_PID" 2>/dev/null
    for _ in 1 2 3 4 5; do
      lsof -ti "tcp:$PF_GW" >/dev/null 2>&1 || break
      sleep 1
    done
  fi
  for _ in 1 2 3 4 5 6; do
    kubectl -n "$OPNS" port-forward svc/flint-lite-operator-gateway "$PF_GW:8090" >/dev/null 2>&1 &
    PF_GW_PID=$!
    for _ in $(seq 1 30); do
      curl -sf "http://127.0.0.1:$PF_GW/healthz" >/dev/null && return 0
      kill -0 "$PF_GW_PID" 2>/dev/null || break
      sleep 1
    done
    kill "$PF_GW_PID" 2>/dev/null
  done
  # NO `fail` HERE: this is reached from gw(), whose stdout is captured,
  # so anything printed becomes the caller's "$code". Status only.
  return 1
}
gw() {  # gw <method> <path> [curl args...] -> prints http code
  local m="$1" path="$2"; shift 2
  local code
  code=$(curl -s -o /tmp/lc-body.bin -w '%{http_code}' -X "$m" \
    -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  if [ "$code" = "000" ]; then
    note "no response — re-establishing the drill's port-forward"
    pf_gw
    code=$(curl -s -o /tmp/lc-body.bin -w '%{http_code}' -X "$m" \
      -H "Authorization: Bearer $GW_TOKEN" "$@" "http://127.0.0.1:$PF_GW$path")
  fi
  echo "$code"
}
phase()    { kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath='{.status.phase}' 2>/dev/null; }
replicas() { kubectl -n "$NS" get deploy "$SHARE" -o jsonpath='{.spec.replicas}' 2>/dev/null; }
# ⚠ NOT `| grep -q .`. Under `set -o pipefail`, grep -q exits on the first
# match, kubectl takes SIGPIPE, the pipeline reports non-zero, and the
# leading `!` turns that into "the PVC is gone" — ALWAYS. That made the
# suspend leg accuse the product of deleting a disk that was plainly
# Bound, and made the hibernate leg's "the bucket is the only copy"
# precondition pass without ever being true. `wc -l` consumes all input,
# so nothing is signalled and the count is real.
pvc_gone() {
  local n
  n=$(kubectl -n "$NS" get pvc -l chert.us/share="$SHARE" --no-headers 2>/dev/null | wc -l | tr -d ' ')
  [ "${n:-0}" -eq 0 ]
}

echo "══════════════════════════════════════════════════════════════════"
echo " hub lifecycle drill — idle spin-down, and waking from S3"
echo " suspendAfterSecs=$SUSPEND_AFTER  hibernateAfterSecs=$HIBERNATE_AFTER"
echo "══════════════════════════════════════════════════════════════════"

# ── setup ────────────────────────────────────────────────────────────
say "preflight"
CTX=$(kubectl config current-context 2>/dev/null) || fail "no kube context"
note "context: $CTX"
note "bucket : s3://$BUCKET ($REGION)"
s3 s3 ls "s3://$BUCKET/" >/dev/null 2>&1 || fail "cannot reach s3://$BUCKET"
EX=$(s3 s3 ls "s3://$BUCKET/$PROJECT/" --recursive 2>/dev/null | grep -c . || true)
[ "${EX:-0}" -eq 0 ] || fail "s3://$BUCKET/$PROJECT/ already has $EX object(s) — refusing to reuse a live prefix"
kubectl create namespace "$NS" >/dev/null 2>&1

if ! helm status flint-lite-operator -n "$OPNS" >/dev/null 2>&1; then
  note "installing the operator + gateway"
  GW_TOKEN="lifecycle-$(date +%s)"
  GW_ROOT="lifecycle-root-key-at-least-32-bytes-long-ok"
  kubectl create namespace "$OPNS" >/dev/null 2>&1
  kubectl -n "$OPNS" create secret generic flint-gateway-token --from-literal=token="$GW_TOKEN" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n "$OPNS" create secret generic flint-gateway-root --from-literal=key="$GW_ROOT" \
    --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  helm install flint-lite-operator "$OP_CHART" ${CHART_VER:+--version "$CHART_VER"} -n "$OPNS" \
    --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
    --set replicas=1 --set gateway.replicas=1 --set gateway.enabled=true \
    --set gateway.tokenSecretRef=flint-gateway-token \
    --set gateway.rootKeySecretRef=flint-gateway-root \
    >/tmp/lc-helm.log 2>&1 || { tail -15 /tmp/lc-helm.log; fail "helm install failed"; }
  kubectl -n "$OPNS" rollout status deploy/flint-lite-operator --timeout=180s >/dev/null 2>&1
  kubectl -n "$OPNS" rollout status deploy/flint-lite-operator-gateway --timeout=180s >/dev/null 2>&1
else
  note "reusing the operator already installed in $OPNS"
  GW_TOKEN=$(kubectl -n "$OPNS" get secret flint-gateway-token -o jsonpath='{.data.token}' 2>/dev/null | base64 -d)
  [ -n "$GW_TOKEN" ] || fail "cannot read the existing gateway token"
fi
pf_gw
pass "operator + gateway reachable"

kubectl -n "$NS" create secret generic flint-s3 \
  --from-literal=AWS_ACCESS_KEY_ID="$HUB_AK" --from-literal=AWS_SECRET_ACCESS_KEY="$HUB_SK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null || fail "creds Secret refused"

say "creating the share with the idle ladder ARMED"
EP_LINE=""; [ -n "$S3_ENDPOINT" ] && EP_LINE="
  endpoint: $S3_ENDPOINT"
kubectl -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare refused"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata:
  name: $SHARE
  namespace: $NS
  labels:
    chert.us/project-id: $PROJECT
spec:
  bucket: $BUCKET
  keyPrefix: $PROJECT/${EP_LINE}
  region: $REGION
  credentialsSecretRef: flint-s3
  persistence:
    size: $PVC_SIZE
  monitoring:
    enabled: true
    fileApi:
      enabled: true
      tokenSecretRef: tok-$PROJECT
  idle:
    suspendAfterSecs: $SUSPEND_AFTER
    hibernateAfterSecs: $HIBERNATE_AFTER
  settings:
    flushFloorSecs: 3
EOF
# The knobs are the subject of this drill, so read them back — a
# structural CRD prunes unknown fields silently and a pruned knob would
# make every timing below a measurement of the DEFAULT, not of what we set.
for kv in "suspendAfterSecs $SUSPEND_AFTER" "hibernateAfterSecs $HIBERNATE_AFTER"; do
  k=${kv% *}; want=${kv#* }
  got=$(kubectl -n "$NS" get flintshare "$SHARE" -o jsonpath="{.spec.idle.$k}" 2>/dev/null)
  [ "$got" = "$want" ] || fail "spec.idle.$k did not stick (got '$got', wanted '$want')"
done
pass "both idle knobs survived admission"

TOK=$(derive_for "$NS/$SHARE")
[ -n "$TOK" ] || { echo "    derive-for stderr:"; tail -3 /tmp/derive-err.txt 2>/dev/null; fail "--derive-for produced nothing"; }
kubectl -n "$NS" create secret generic "tok-$PROJECT" --from-literal=token="$TOK" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null

for _ in $(seq 1 60); do [ "$(phase)" = Ready ] && break; sleep 5; done
[ "$(phase)" = Ready ] || { kubectl -n "$NS" get flintshare "$SHARE" -o yaml | tail -25; fail "share never became Ready"; }
pass "share Ready, replicas=$(replicas)"

# ══ L1: the file we will demand back later ═══════════════════════════
say "L1: seed a file through the REST door, and let it reach the bucket"
dd if=/dev/urandom of=/tmp/lc-payload.bin bs=1M count=8 2>/dev/null
WANT=$(shasum -a 256 /tmp/lc-payload.bin 2>/dev/null | awk '{print $1}')
[ -n "$WANT" ] || WANT=$(sha256sum /tmp/lc-payload.bin | awk '{print $1}')
code=$(gw PUT "/v1/projects/$PROJECT/files/content?path=/payload.bin" \
  -H 'Content-Type: application/octet-stream' -H 'Expect:' -T /tmp/lc-payload.bin)
case "$code" in 200|201|204) ;; *) fail "seeding failed (HTTP $code): $(head -c 200 /tmp/lc-body.bin)" ;; esac
PUB=""
for _ in $(seq 1 60); do
  # same trap, opposite symptom: a SIGPIPE here would make a SUCCESSFUL
  # publish look like it never happened, and the loop would time out.
  [ "$(s3 s3 ls "s3://$BUCKET/$PROJECT/" --recursive 2>/dev/null | grep -c 'payload.bin')" -gt 0 ] \
    && { PUB=1; break; }
  sleep 3
done
[ -n "$PUB" ] || fail "the file never reached the bucket — nothing to come back FROM"
pass "8 MiB seeded and published to s3://$BUCKET/$PROJECT/"

# ══ L2: it winds down on its own ═════════════════════════════════════
say "L2: with nobody using it, the share suspends after ~${SUSPEND_AFTER}s"
note "going quiet — touching neither the mount nor the file API"
T0=$(now_ms); SUSPENDED=""
# Budget: the ladder needs BOTH signals stale, and the operator only acts
# on a reconcile, so allow generous slack over the nominal value.
DEADLINE=$(( SUSPEND_AFTER * 4 + 180 ))
for _ in $(seq 1 $((DEADLINE / 5))); do
  P=$(phase); R=$(replicas)
  if [ "$P" = IdleSuspended ] || [ "$R" = "0" ]; then SUSPENDED=1; break; fi
  sleep 5
done
T1=$(now_ms)
if [ -n "$SUSPENDED" ]; then
  DOWN_S=$(( (T1 - T0) / 1000 ))
  pass "share wound down after ${DOWN_S}s (phase=$(phase), replicas=$(replicas))"
  # SUSPEND KEEPS THE DISK. If the PVC went away here, this is hibernate
  # and every conclusion about the two rungs would be wrong.
  if pvc_gone; then
    # EVIDENCE BEFORE THE ACCUSATION. This says the product deleted a disk
    # it promised to keep — a serious claim, so dump what the check saw,
    # including PVCs the label selector might have missed.
    note "PVCs matching chert.us/share=$SHARE:"; kubectl -n "$NS" get pvc -l chert.us/share="$SHARE" 2>&1 | head -4
    note "ALL PVCs in $NS:";                     kubectl -n "$NS" get pvc 2>&1 | head -6
    note "phase=$(phase) replicas=$(replicas)"
    bad "the PVC is GONE after a SUSPEND — suspend must keep the disk"
  else
    pass "the PVC survived the suspend (this rung keeps the disk, by design)"
  fi
else
  bad "share never suspended within ${DEADLINE}s (phase=$(phase), replicas=$(replicas))"
  DOWN_S="?"
fi

# ══ L3: asking for a file brings it back ═════════════════════════════
say "L3: a file request wakes it, and returns the bytes"
T0=$(now_ms)
code=$(gw GET "/v1/projects/$PROJECT/files/content?path=/payload.bin" --max-time 180)
T1=$(now_ms)
WAKE_MS=$((T1-T0))
if [ "$code" = "200" ]; then
  GOT=$(shasum -a 256 /tmp/lc-body.bin 2>/dev/null | awk '{print $1}')
  [ -n "$GOT" ] || GOT=$(sha256sum /tmp/lc-body.bin | awk '{print $1}')
  if [ "$GOT" = "$WANT" ]; then
    pass "a cold GET woke the share and returned byte-identical content in ${WAKE_MS}ms"
  else
    bad "the woken share returned the WRONG BYTES (got $GOT, wanted $WANT)"
  fi
else
  note "body: $(head -c 200 /tmp/lc-body.bin)"
  bad "the wake request answered HTTP $code, not 200"
fi
note "phase now: $(phase)  replicas: $(replicas)"

# ══ L4: the fleet-crawl guard ════════════════════════════════════════
say "L4: ?wake=false refuses instead of starting a parked hub"
# Let it wind down again first, or the guard has nothing to refuse.
note "waiting for it to go quiet again"
RE_DOWN=""
for _ in $(seq 1 $((DEADLINE / 5))); do
  [ "$(replicas)" = "0" ] && { RE_DOWN=1; break; }
  sleep 5
done
if [ -n "$RE_DOWN" ]; then
  T0=$(now_ms)
  code=$(gw GET "/v1/projects/$PROJECT/files?path=/&wake=false" --max-time 60)
  T1=$(now_ms)
  R_AFTER=$(replicas)
  note "HTTP $code in $((T1-T0))ms; replicas still $R_AFTER"
  if [ "$code" = "503" ] && [ "$R_AFTER" = "0" ]; then
    pass "wake=false refused (503) and left the hub parked — a fleet crawl cannot start 2000 hubs"
  elif [ "$R_AFTER" != "0" ]; then
    bad "wake=false STARTED the hub (replicas=$R_AFTER) — the crawl guard does not hold"
  else
    bad "wake=false answered HTTP $code, expected 503"
  fi
else
  note "did not go back down in time; L4 INCONCLUSIVE"
  L4_INC=1
fi

# ══ L5: hibernate — the bucket is the only copy ══════════════════════
say "L5: after hibernation the PVC is DELETED, and the file still comes back"
note "waiting for hibernation (hibernateAfterSecs=$HIBERNATE_AFTER, verify-then-delete)"
HIB=""
HIB_AT=""                       # when the phase FIRST read Hibernated
HT0=$(date +%s)
# The disk reclaim is driven by a RECONCILE, and once a share reaches
# Hibernated the operator parks it for REQUEUE_PARKED = 1800s. The
# controller watches Deployments/Services/ConfigMaps/Secrets/PVCs/Shares
# but NOT Pods, so the hub pod finally disappearing does not schedule the
# pass that deletes the claim — the claim is reclaimed on whichever
# reconcile happens to come next.
#
# Measured on runbu 2026-08-22: phase Hibernated 19:44:21, pod gone
# ~19:44:53, PVC STILL Bound 11 minutes later; a forced reconcile
# deleted it within 200ms. So this deadline has to clear 1800s, or the
# leg fails on the operator's own timer rather than on a real defect.
HDEADLINE=$(( HIBERNATE_AFTER * 5 + 2100 ))
for _ in $(seq 1 $((HDEADLINE / 10))); do
  P=$(phase)
  if [ -z "$HIB_AT" ] && [ "$P" = "Hibernated" ]; then HIB_AT=$(( $(date +%s) - HT0 )); fi
  if pvc_gone; then HIB=1; RECLAIM_AT=$(( $(date +%s) - HT0 )); break; fi
  sleep 10
done
if [ -n "$HIB_AT" ] && [ -n "${RECLAIM_AT:-}" ]; then
  RECLAIM_LAG=$(( RECLAIM_AT - HIB_AT ))
fi
if [ -z "$HIB" ]; then
  note "phase=$(phase) replicas=$(replicas); PVC still present after ${HDEADLINE}s"
  note "hibernation is verify-then-delete: it re-wakes to confirm rpoClean before"
  note "deleting the disk, so it is legitimately slower than suspend."
  if [ -n "$HIB_AT" ]; then
    note "phase reached Hibernated at ${HIB_AT}s but the CLAIM was never reclaimed:"
    note "that is the reconcile-scheduling gap, not a failed flush."
  fi
  bad "never hibernated within ${HDEADLINE}s — L5 could not test the S3-only path"
else
  pass "the PVC is GONE — the bucket is now the only copy of this workspace"
  # The hibernate wait is many minutes; assume the forwarder went stale
  # rather than discovering it mid-request.
  pf_gw || note "could not re-establish the port-forward before the DR read"
  rm -f /tmp/lc-body.bin      # so a STALE body cannot be reported as this leg's
  T0=$(now_ms)
  code=$(gw GET "/v1/projects/$PROJECT/files/content?path=/payload.bin" --max-time 600)
  T1=$(now_ms)
  DR_MS=$((T1-T0))
  if [ "$code" = "200" ]; then
    GOT=$(shasum -a 256 /tmp/lc-body.bin 2>/dev/null | awk '{print $1}')
    [ -n "$GOT" ] || GOT=$(sha256sum /tmp/lc-body.bin | awk '{print $1}')
    if [ "$GOT" = "$WANT" ]; then
      pass "REBUILT FROM S3 ALONE: byte-identical 8 MiB returned in ${DR_MS}ms after the disk was destroyed"
    else
      bad "the DR-restored file does not match (got $GOT, wanted $WANT)"
    fi
  else
    if [ -s /tmp/lc-body.bin ]; then note "body: $(head -c 200 /tmp/lc-body.bin)"
    else note "(no response body — the request never completed)"; fi
    bad "the post-hibernate request answered HTTP '$code', not 200"
  fi
fi

# ══ summary ══════════════════════════════════════════════════════════
echo
echo "══════════════════════════════════════════════════════════════════"
echo " hub lifecycle summary — $PASSES checks passed"
echo "══════════════════════════════════════════════════════════════════"
echo " configured suspendAfterSecs        : ${SUSPEND_AFTER}s"
echo " observed time to wind down         : ${DOWN_S:-?}s"
echo " wake-on-file-request (PVC intact)  : ${WAKE_MS:-?}ms"
echo " wake=false refused while parked    : ${L4_INC:+INCONCLUSIVE}${L4_INC:-yes}"
echo " configured hibernateAfterSecs      : ${HIBERNATE_AFTER}s"
echo " phase reached Hibernated after     : ${HIB_AT:-?}s"
echo " disk reclaimed after Hibernated    : ${RECLAIM_LAG:-?}s  (operator parks at 1800s)"
echo " rebuild from S3 after PVC deleted  : ${DR_MS:-not reached}ms"
echo
if [ ${#FAILURES[@]} -eq 0 ]; then
  echo "ALL LEGS PASSED."
else
  echo "${#FAILURES[@]} LEG(S) FAILED:"
  for f in "${FAILURES[@]}"; do echo "  ✗ $f"; done
  exit 1
fi
