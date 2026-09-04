#!/usr/bin/env bash
# NOT RETIRED — re-checked 2026-09-03 against the CSI cutover (S12).
# An earlier banner here declared this path retired. That was WRONG, and
# a wrong banner is the worse failure: a suite nobody runs because it
# says not to. This rig never used the lean webhook. It creates no
# FlintLeanWorkspace, reads no chert.us/lean-workspace label, and needs
# no operator: its pods are hand-authored in chaos.yaml (chaos-a, chaos-k, chaos-k2, chaos-s) with an
# EXPLICIT `sync` container, and the drill execs flint-sync verbs into
# them with per-leg env and per-leg subtrees.
#
# What the CSI cutover changed is DELIVERY — how a workspace reaches a
# pod — which this suite does not test and never did. Delivery is
# drilled by s3csi/e2e/run-s3csi.sh (S11, S12, S13) and, across
# clusters, s3csi/e2e/multi/run-multi.sh (M3). What C1-C12 tests is the
# bucket PROTOCOL, and delivery does not change it.
#
# So design §10.2 S12's "re-target every `kubectl exec -c flint-sync`
# step at the worker pod in flint-workers" does not apply here: there is
# no injected container to re-target, and a worker pod could not host
# these legs anyway — one CSI volume is one prefix, while every leg here
# needs its own subtree, and reset_pods kills the resident syncer, which
# under CSI would take PID 1 of the worker with it.
# The flint-lean CHAOS drill (plan §5 Phase 6, the kind-runnable half).
#
# run.sh and run-chart.sh prove the happy path: injection, gate,
# publish, refusal. This drill proves what happens when the happy path
# is INTERRUPTED — the legs the formal model either abstracts away
# (the atomic scan, the poll protocol) or explicitly cannot represent
# (the two-consecutive-scans rule, real crash timing), plus the two
# gateway-side mechanisms P5 rests on.
#
# EVERY leg carries an anti-vacuity guard. The lite drill's lesson was
# that 24 of 41 proposed legs would have PASSED IF BROKEN; here a leg
# that cannot observe its own precondition FAILS rather than passing
# quietly. Concretely: a crash leg that did not land mid-barrier is a
# FAILED leg, not a green one, because the thing it claims to test
# never happened.
#
# Prereqs: kind cluster `flint-lean-chaos` with flint-sync:e2e and
# flint-lean-gateway:e2e loaded; minio.yaml + chaos.yaml applied.
# Runtime ~6-8 min (two legs wait out a 6-quiet-poll takeover).
set -u
cd "$(dirname "$0")"

CTX=${CTX:-kind-flint-lean-chaos}
K="kubectl --context $CTX"
BUCKET=agentws
GW=http://lean-gateway.flint-system.svc:8091
TOK=chaos-drill-token-0123456789

PASS=0
FAILED=0
NOTES=()

note() { NOTES+=("$1"); echo "  note: $1"; }
ok()   { echo "  ok: $1"; }
bad()  { echo "  BAD: $1"; }
has()  { [ "$(printf '%s' "$2" | grep -c -- "$1")" -gt 0 ]; }

leg() {
  local name=$1; shift
  echo
  echo "── $name"
  if "$@"; then
    PASS=$((PASS + 1)); echo "  PASS"
  else
    FAILED=$((FAILED + 1)); echo "  FAIL"
  fi
}

# ── rig helpers ──────────────────────────────────────────────────────
# flint-sync in <pod>, with the leg's own prefix/root (legs never share
# a subtree: a crash leg's debris must not become the next leg's state).
sy() {
  local pod=$1 prefix=$2 root=$3; shift 3
  $K exec "$pod" -c sync -- /bin/sh -c \
    "FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root /usr/local/bin/flint-sync $*" 2>&1
}
# Same, detached: the process must outlive the exec session so the
# script can kill/stop it mid-barrier.
sy_bg() {
  local pod=$1 prefix=$2 root=$3 log=$4; shift 4
  $K exec "$pod" -c sync -- /bin/sh -c \
    "nohup env FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root \
     /usr/local/bin/flint-sync $* > $log 2>&1 & echo bg-started" > /dev/null 2>&1
}
inpod() { local pod=$1; shift; $K exec "$pod" -c sync -- /bin/sh -c "$*" 2>/dev/null; }
# Names are ZERO-PADDED on purpose. The upload set is a BTreeSet walked
# at fan-out 1, so uploads happen in lexicographic order — with padding
# that is also numeric order, which turns "did the kill land inside the
# upload loop?" into two cheap HEADs (an early key present, the last key
# absent) instead of a race against a fixed sleep.
mkfiles() { # <pod> <dir> <n> <tag>  ⇒ <tag>0001.txt … <tag>NNNN.txt
  inpod "$1" "mkdir -p $2 && awk -v d=$2 -v n=$3 -v t=$4 \
    \"BEGIN{for(i=1;i<=n;i++){fn=sprintf(\\\"%s/%s%04d.txt\\\", d, t, i); print t \\\"-\\\" i > fn; close(fn)}}\" && echo made"
}
pad() { printf '%04d' "$1"; }
# `pidof` alone is the WRONG liveness test here and it cost ~7 minutes a
# run to learn: PID 1 in these containers is `sleep infinity`, which
# never reaps, so a backgrounded flint-sync that has exited sits as a
# ZOMBIE and pidof keeps finding it forever. Read the state field.
proc_alive() { # <pod> <process name>
  inpod "$1" "for p in \$(pidof $2 2>/dev/null); do \
                s=\$(awk '{print \$3}' /proc/\$p/stat 2>/dev/null); \
                [ \"\$s\" != Z ] && exit 0; done; exit 1"
}
await_exit() { # <pod> <process name> <iters>
  local i
  for i in $(seq 1 "$3"); do
    proc_alive "$1" "$2" || return 0
    sleep 2
  done
  return 1
}
# flint-sync against a DIFFERENT endpoint — the proxy-outage lever.
# Scaling MinIO to zero would destroy the rig's data (its /data is the
# container's writable layer); an unreachable endpoint reproduces
# "the proxy is not answering" without losing the bucket.
sy_ep() { # <pod> <prefix> <root> <endpoint> <args…>
  local pod=$1 prefix=$2 root=$3 ep=$4; shift 4
  $K exec "$pod" -c sync -- /bin/sh -c \
    "FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root FLINT_SYNC_ENDPOINT=$ep \
     /usr/local/bin/flint-sync $*" 2>&1
}
# Block until an object appears (bounded). Replaces sleep-and-hope: the
# drill kills the barrier when it can SEE the barrier is mid-flight.
wait_key() { # <prefix> <relative key> <iters>
  local i
  for i in $(seq 1 "$3"); do
    objexists "$1/files/$2" && return 0
    sleep 0.2
  done
  return 1
}

# The oracle reads the bucket DIRECTLY (mc), never through the sidecar's
# own code — otherwise one bug can hide another.
mcx()      { $K -n flint-system exec mc -- "$@" 2>/dev/null; }
objcat()   { mcx mc cat "m/$BUCKET/$1"; }
objexists(){ mcx mc stat "m/$BUCKET/$1" > /dev/null 2>&1; }
putobj()   { printf '%s' "$2" | $K -n flint-system exec -i mc -- mc pipe "m/$BUCKET/$1" > /dev/null 2>&1; }
allkeys()  { mcx mc ls --recursive --json "m/$BUCKET/$1/" | jq -r --arg p "$1/" 'select(.key)|$p + .key'; }
# Resolve the manifest THROUGH the pointer: `.flint/lean/current` is the
# only mutable metadata object and it NAMES the write-once generation
# holding the entries. The pre-pointer `.flint/lean/manifest` key is
# tried LAST — after migration it holds a refusal doc with no
# `.entries`. Reading it first is not merely stale, it is VACUOUS: a
# missing object answers 0/false, so assertions pass by reading nothing.
mbody() {
    local c k
    c=$(objcat "$1/.flint/lean/current")
    if [ -n "$c" ]; then
        k=$(printf '%s' "$c" | jq -r '.entries_key // empty')
        [ -z "$k" ] && return 1
        objcat "$k"
        return
    fi
    objcat "$1/.flint/lean/manifest"
}
manif()    { mbody "$1"; }

gw_get()  { $K -n flint-system exec curl -- sh -c \
              "curl -sS -o /tmp/out -w '%{http_code}' -H 'Authorization: Bearer $TOK' '$GW$1'" 2>/dev/null; }
gw_post() { printf '%s' "$2" | $K -n flint-system exec -i curl -- sh -c \
              "cat > /tmp/body && curl -sS -o /tmp/out -w '%{http_code}' -X POST \
               -H 'Authorization: Bearer $TOK' -H 'Content-Type: application/json' \
               --data-binary @/tmp/body '$GW$1'" 2>/dev/null; }
gw_put()  { printf '%s' "$2" | $K -n flint-system exec -i curl -- sh -c \
              "cat > /tmp/body && curl -sS -o /tmp/out -D /tmp/hdr -w '%{http_code}' -X PUT \
               -H 'Authorization: Bearer $TOK' --data-binary @/tmp/body '$GW$1'" 2>/dev/null; }
gw_body() { $K -n flint-system exec curl -- cat /tmp/out 2>/dev/null; }
gw_hdr()  { $K -n flint-system exec curl -- cat /tmp/hdr 2>/dev/null; }
gw_epoch(){ gw_get "/lean/v1/$1/status" > /dev/null; gw_body | jq -r '.epoch'; }
gw_now()  { gw_get "/lean/v1/$1/status" > /dev/null; gw_body | jq -r '.now_unix'; }
gw_healthy() { # <iters>
  local i c
  for i in $(seq 1 "$1"); do
    c=$($K -n flint-system exec curl -- sh -c \
        "curl -sS -o /dev/null -w '%{http_code}' '$GW/healthz'" 2>/dev/null)
    [ "$c" = "200" ] && return 0
    sleep 1
  done
  return 1
}

# Every key the manifest cites must resolve to a live object.
dangling() { # <prefix> -> count
  local m present cited
  m=$(manif "$1")
  cited=$(printf '%s' "$m" | jq -r '.entries[].key' | sort)
  present=$(allkeys "$1" | sort)
  comm -23 <(printf '%s\n' "$cited") <(printf '%s\n' "$present") | grep -c .
}

wait_restart() { # <pod> <old count>
  local i rc
  for i in $(seq 1 60); do
    rc=$($K get pod "$1" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    [ -n "$rc" ] && [ "$rc" -gt "$2" ] && { echo "$rc"; return 0; }
    sleep 2
  done
  echo "$2"; return 1
}

# ─────────────────────────────────────────────────────────────────────
# C1  Process crash mid-barrier, then recovery over the SAME emptyDir.
#     Claim: a crashed barrier leaves the bucket coherent (uncited
#     orphans, never a dangling manifest), and the retry recognizes its
#     own crashed PUTs (intent journal ⇒ AdoptOwn) instead of parking
#     them as foreign.
# ─────────────────────────────────────────────────────────────────────
c1_crash_midbarrier() {
  local P=tenants/c1 R=/work/c1 N=8000
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  mkfiles chaos-a $R $N f > /dev/null
  sy_bg chaos-a $P $R /work/c1.log barrier
  wait_key "$P" "f0200.txt" 150 || { bad "the barrier never reached its 200th upload"; return 1; }
  inpod chaos-a "kill -9 \$(pidof flint-sync) 2>/dev/null; echo k" > /dev/null
  sleep 2

  local landed
  landed=$(allkeys "$P/files" | grep -c .)
  # ANTI-VACUITY: the kill has to have landed strictly inside the upload
  # loop. Too early (nothing uploaded) or too late (manifest committed)
  # and the leg tested nothing — that is a failure, not a pass.
  [ "$landed" -gt 0 ] || { bad "kill landed before the first upload — leg vacuous"; return 1; }
  if objexists "$P/files/f$(pad $N).txt"; then
    bad "the last upload ($N) completed before the kill — leg vacuous"; return 1
  fi
  # The CAS that makes a publish VISIBLE is the pointer's, so that is
  # the object whose absence proves the kill landed mid-barrier. Testing
  # the retired `manifest` key would find nothing under the pointer
  # layout and pass this guard unconditionally — a vacuity check that
  # had itself gone vacuous.
  if objexists "$P/.flint/lean/current"; then
    bad "the manifest pointer CAS completed — the kill missed the barrier, leg vacuous"; return 1
  fi
  ok "crash landed mid-barrier: $landed/$N objects up, no manifest (uncited orphans)"

  local out
  out=$(sy chaos-a $P $R barrier)
  has "up=$N" "$out"    || { bad "recovery barrier did not publish all $N: $out"; return 1; }
  has "parked=0" "$out" || { bad "recovery PARKED paths — AdoptOwn did not recognize its own crashed PUTs: $out"; return 1; }
  ok "recovery barrier: $(printf '%s' "$out" | tail -1)"

  local cited d
  cited=$(manif "$P" | jq -r '.entries|keys|length')
  [ "$cited" = "$N" ] || { bad "manifest cites $cited of $N"; return 1; }
  d=$(dangling "$P")
  [ "$d" -eq 0 ] || { bad "$d cited keys have no object (dangling manifest)"; return 1; }
  ok "manifest cites all $N, zero dangling citations"
}

# ─────────────────────────────────────────────────────────────────────
# C2  Pod loss mid-barrier ⇒ a fresh pod takes over.
#     Claim: loss is exactly the RPO — everything published before the
#     kill survives, everything after it is gone, and the successor's
#     checkout reproduces precisely the published set. Rotation bumps
#     seq WITHOUT changing entries.
# ─────────────────────────────────────────────────────────────────────
c2_podloss_takeover() {
  local P=tenants/c2 R=/work/c2 BASE=6 BURST=8000
  sy chaos-k $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  mkfiles chaos-k $R $BASE base > /dev/null
  local out
  out=$(sy chaos-k $P $R barrier)
  has "up=$BASE" "$out" || { bad "base barrier: $out"; return 1; }
  local seq0
  seq0=$(manif "$P" | jq -r '.seq')
  ok "published the RPO-covered set: $BASE files at seq $seq0"

  mkfiles chaos-k $R $BURST burst > /dev/null
  sy_bg chaos-k $P $R /work/c2.log barrier
  wait_key "$P" "burst0200.txt" 150 || { bad "the barrier never reached its 200th burst upload"; return 1; }
  $K delete pod chaos-k --force --grace-period=0 --wait=false > /dev/null 2>&1
  sleep 3

  local orphans
  orphans=$(allkeys "$P/files" | grep -c 'burst')
  # ANTI-VACUITY: if no burst object landed, the pod died before the
  # barrier did anything and the takeover has nothing to be correct about.
  [ "$orphans" -gt 0 ] || { bad "pod died before any burst upload — leg vacuous"; return 1; }
  if objexists "$P/files/burst$(pad $BURST).txt"; then
    bad "the whole burst uploaded before the kill — leg vacuous"; return 1
  fi
  ok "pod lost mid-barrier: $orphans/$BURST burst objects orphaned in the bucket"

  # The successor: fresh emptyDir ⇒ fresh incarnation ⇒ it MUST wait out
  # the quiet polls rather than self-recognize.
  local cout
  cout=$(sy chaos-k2 $P $R checkout)
  has "quiet" "$cout" || { bad "successor did NOT wait out the standing lease (self-recognized?): $cout"; return 1; }
  has "materialized" "$cout" || { bad "successor checkout produced no report: $cout"; return 1; }
  ok "successor waited out the standing lease, then claimed"

  local tree
  tree=$(inpod chaos-k2 "cd $R && ls | sort | tr '\n' ' '")
  local want="base0001.txt base0002.txt base0003.txt base0004.txt base0005.txt base0006.txt "
  [ "$tree" = "$want" ] || { bad "successor tree is '$tree', want '$want' (RPO not exact)"; return 1; }
  ok "successor tree is EXACTLY the published set — loss equals the RPO"

  local seq1 cited
  seq1=$(manif "$P" | jq -r '.seq')
  cited=$(manif "$P" | jq -r '.entries|keys|length')
  [ "$seq1" -gt "$seq0" ] || { bad "takeover did not rotate the manifest (seq $seq0 -> $seq1)"; return 1; }
  [ "$cited" = "$BASE" ] || { bad "rotation changed the entry set ($cited entries, want $BASE)"; return 1; }
  ok "rotation bumped seq $seq0 -> $seq1 content-identical ($cited entries)"

  local d
  d=$(dangling "$P")
  [ "$d" -eq 0 ] || { bad "$d dangling citations after takeover"; return 1; }
  ok "zero dangling citations after takeover"
}

# ─────────────────────────────────────────────────────────────────────
# C3  A STOPPED straggler resumes after it has been deposed.
#     Claim (Inv_NoStragglerInstall): its manifest CAS never lands.
#     Also MEASURES the known P5 residual — whether its data PUTs still
#     land after deposal, which is what proxy-side epoch enforcement
#     would have to close.
# ─────────────────────────────────────────────────────────────────────
c3_straggler_after_takeover() {
  local P=tenants/c3 R=/work/c3 BASE=4 BURST=8000
  sy chaos-s $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  mkfiles chaos-s $R $BASE base > /dev/null
  sy chaos-s $P $R barrier > /dev/null || { bad "base barrier"; return 1; }
  local seq0
  seq0=$(manif "$P" | jq -r '.seq')

  mkfiles chaos-s $R $BURST burst > /dev/null
  sy_bg chaos-s $P $R /work/c3.log barrier
  wait_key "$P" "burst0200.txt" 150 || { bad "the barrier never reached its 200th upload"; return 1; }
  inpod chaos-s "kill -STOP \$(pidof flint-sync); echo stopped" > /dev/null
  local frozen
  frozen=$(allkeys "$P/files" | grep -c 'burst')
  [ "$frozen" -gt 0 ] || { bad "straggler stopped before any upload — leg vacuous"; return 1; }
  if objexists "$P/files/burst$(pad $BURST).txt"; then
    bad "straggler finished before the STOP — leg vacuous"; return 1
  fi
  ok "straggler frozen mid-barrier at $frozen/$BURST uploads"

  local cout
  cout=$(sy chaos-s2 $P $R checkout)
  has "quiet" "$cout" || { bad "successor did not take over: $cout"; return 1; }
  local seq1
  seq1=$(manif "$P" | jq -r '.seq')
  [ "$seq1" -gt "$seq0" ] || { bad "no rotation on takeover (seq $seq0 -> $seq1)"; return 1; }
  ok "successor deposed the straggler and rotated (seq $seq0 -> $seq1)"

  inpod chaos-s "kill -CONT \$(pidof flint-sync); echo resumed" > /dev/null
  await_exit chaos-s flint-sync 90 || { bad "the thawed straggler never exited"; return 1; }
  local log
  log=$(inpod chaos-s "cat /work/c3.log")
  has "fenced" "$log" || { bad "the resumed straggler did NOT fence: $log"; return 1; }
  ok "resumed straggler self-fenced: $(printf '%s' "$log" | grep fenced | head -1)"

  local seq2 cited_burst
  seq2=$(manif "$P" | jq -r '.seq')
  cited_burst=$(manif "$P" | jq -r '.entries|keys[]' | grep -c 'burst')
  [ "$seq2" = "$seq1" ] || { bad "the straggler's manifest CAS LANDED (seq $seq1 -> $seq2)"; return 1; }
  [ "$cited_burst" -eq 0 ] || { bad "$cited_burst straggler paths are cited — straggler install"; return 1; }
  ok "straggler install refused: manifest still at seq $seq2, zero straggler citations"

  # The residual, measured rather than assumed.
  local thawed
  thawed=$(allkeys "$P/files" | grep -c 'burst')
  if [ "$thawed" -gt "$frozen" ]; then
    note "P5 data-plane residual CONFIRMED: the deposed straggler landed $((thawed - frozen)) more data PUTs after rotation ($frozen -> $thawed). The control plane held; only proxy-side epoch enforcement closes the data path."
  else
    note "no post-deposal data PUTs observed this run ($frozen -> $thawed) — the residual is timing-dependent, not absent"
  fi
}

# ─────────────────────────────────────────────────────────────────────
# C4  Container restart over a live tree, then the two-scan delete rule.
#     Claim A (Inv_NoResurrection): a restart never re-materializes, so
#     an unpublished delete is not resurrected.
#     Claim B: a delete needs TWO consecutive scans — the rename-vs-walk
#     guard the formal model explicitly cannot represent.
# ─────────────────────────────────────────────────────────────────────
c4_restart_and_two_scan_delete() {
  local P=tenants/c4 R=/work/c4
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo a > $R/a.txt; echo b > $R/b.txt; echo c > $R/c.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }
  objexists "$P/files/b.txt" || { bad "b.txt never published"; return 1; }
  ok "published a/b/c"

  inpod chaos-a "rm -f $R/b.txt; echo rm" > /dev/null
  local rc0 rc1
  rc0=$($K get pod chaos-a -o jsonpath='{.status.containerStatuses[0].restartCount}')
  inpod chaos-a "touch /work/DIE" > /dev/null
  rc1=$(wait_restart chaos-a "$rc0")
  # ANTI-VACUITY: without a real restart this leg asserts nothing.
  [ "$rc1" -gt "$rc0" ] || { bad "container never restarted (restartCount $rc0) — leg vacuous"; return 1; }
  $K wait --for=condition=Ready pod/chaos-a --timeout=120s > /dev/null 2>&1
  ok "container restarted over the live emptyDir (restartCount $rc0 -> $rc1)"

  local cout
  cout=$(sy chaos-a $P $R checkout)
  has "live-tree=true" "$cout" || { bad "restart did NOT take the live-tree row: $cout"; return 1; }
  has "0 materialized" "$cout" || { bad "restart re-materialized: $cout"; return 1; }
  local tree
  tree=$(inpod chaos-a "cd $R && ls | sort | tr '\n' ' '")
  [ "$tree" = "a.txt c.txt " ] || { bad "tree is '$tree' — the unpublished delete was RESURRECTED"; return 1; }
  ok "no resurrection: tree is 'a.txt c.txt' after restart"

  sy chaos-a $P $R barrier > /dev/null || { bad "post-restart barrier 1"; return 1; }
  # Claim B: one scan of absence is NOT enough.
  objexists "$P/files/b.txt" || { bad "b.txt deleted after ONE absent scan — the two-scan guard is gone"; return 1; }
  ok "barrier 1: b.txt still in the bucket (first absence only)"

  sy chaos-a $P $R barrier > /dev/null || { bad "post-restart barrier 2"; return 1; }
  if objexists "$P/files/b.txt"; then bad "b.txt survived two absent scans — the delete never happened"; return 1; fi
  local cited
  cited=$(manif "$P" | jq -r '.entries|keys[]' | tr '\n' ' ')
  [ "$cited" = "a.txt c.txt " ] || { bad "manifest cites '$cited' after the delete"; return 1; }
  ok "barrier 2: b.txt GC'd and un-cited — two consecutive scans, exactly"
}

# ─────────────────────────────────────────────────────────────────────
# C5  HITL write vs a dirty local file, end to end.
#     Claim (Inv_HITLDurable): the conflict surfaces and BOTH versions
#     stay recoverable — the bytes, not just the reference.
# ─────────────────────────────────────────────────────────────────────
c5_hitl_conflict() {
  local P=tenants/c5 R=/work/c5 WS=c5
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo v1 > $R/shared.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }
  ok "published shared.txt=v1"

  # The agent edits locally (dirty, unpublished).
  inpod chaos-a "echo from-agent-local > $R/shared.txt; echo w" > /dev/null
  # The human writes the same path through the gateway.
  local code
  code=$(gw_put "/lean/v1/$WS/files/shared.txt" "from-ui-human")
  [ "$code" = "200" ] || { bad "gateway PUT returned $code: $(gw_body)"; return 1; }
  ok "HITL write accepted (object + inbox entry, no manifest edit)"

  local out
  out=$(sy chaos-a $P $R barrier)
  has "consumed=1" "$out" || { bad "barrier did not consume the inbox entry: $out"; return 1; }

  local conflicts pk
  conflicts=$(inpod chaos-a "cat $R/.flint-sync/conflicts.jsonl")
  has "consume-dirty" "$conflicts" || { bad "no consume-dirty conflict recorded: $conflicts"; return 1; }
  pk=$(printf '%s' "$conflicts" | jq -r 'select(.kind=="consume-dirty")|.preserved_key' | tail -1)
  [ -n "$pk" ] && [ "$pk" != "null" ] || { bad "conflict record has no preserved_key — the foreign bytes were not kept"; return 1; }

  local kept live
  kept=$(objcat "$pk")
  live=$(objcat "$P/files/shared.txt")
  # ANTI-VACUITY: the two versions must actually differ, or "both
  # recoverable" is trivially true.
  [ "$kept" != "$live" ] || { bad "preserved and live bytes are identical — nothing was in conflict"; return 1; }
  [ "$kept" = "from-ui-human" ] || { bad "preserved bytes are '$kept', want the HITL version"; return 1; }
  [ "$live" = "from-agent-local" ] || { bad "live bytes are '$live', want the agent version"; return 1; }
  ok "both versions recoverable: live=agent, preserved=$pk holds the HITL bytes"

  # S3 ETags are quoted; mc reports them unquoted. Compare the values,
  # not the quoting convention of whoever printed them.
  local cited_etag actual_etag
  cited_etag=$(manif "$P" | jq -r '.entries["shared.txt"].etag' | tr -d '"')
  actual_etag=$(mcx mc stat --json "m/$BUCKET/$P/files/shared.txt" | jq -r '.etag' | tr -d '"')
  [ "$cited_etag" = "$actual_etag" ] || { bad "manifest cites $cited_etag, object is $actual_etag"; return 1; }
  ok "manifest citation matches the live object"
}

# ─────────────────────────────────────────────────────────────────────
# C6  Per-request epoch validation — P5's teeth.
#     Claim: a stale epoch is refused on EVERY sidecar-facing verb, and
#     the current epoch is admitted (so the 403s are the check firing,
#     not the endpoint being broken).
# ─────────────────────────────────────────────────────────────────────
c6_gateway_epoch_validation() {
  local P=tenants/c6 R=/work/c6 WS=c6
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo one > $R/one.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }

  local E stale now code
  E=$(gw_epoch $WS); now=$(gw_now $WS); stale=$((E - 1))
  [ "$E" -gt 1 ] || { bad "epoch is $E — no room for a stale claim"; return 1; }
  ok "cell is at epoch $E"

  code=$(gw_post "/lean/v1/$WS/window/open" "{\"epoch\":$stale,\"deadline_unix\":$((now + 60))}")
  [ "$code" = "403" ] || { bad "window/open at stale epoch returned $code (want 403): $(gw_body)"; return 1; }
  has "stale-epoch" "$(gw_body)" || { bad "403 but not stale-epoch: $(gw_body)"; return 1; }

  code=$(gw_post "/lean/v1/$WS/inbox/drop" "{\"epoch\":$stale,\"consumed\":[]}")
  [ "$code" = "403" ] || { bad "inbox/drop at stale epoch returned $code (want 403)"; return 1; }

  # The one that matters: a deposed straggler's manifest install.
  code=$(gw_post "/lean/v1/$WS/manifest" \
    "{\"manifest\":{\"seq\":999,\"entries\":{}},\"epoch\":$stale,\"flush_uuid\":\"straggler\"}")
  [ "$code" = "403" ] || { bad "manifest CAS at stale epoch returned $code (want 403): $(gw_body)"; return 1; }
  ok "stale epoch refused on window/open, inbox/drop and manifest"

  # ANTI-VACUITY: the same verbs at the CURRENT epoch must work, or the
  # 403s prove only that the endpoints are broken.
  code=$(gw_post "/lean/v1/$WS/window/open" "{\"epoch\":$E,\"deadline_unix\":$((now + 30))}")
  [ "$code" = "200" ] || { bad "window/open at the CURRENT epoch returned $code — the 403s were vacuous"; return 1; }
  code=$(gw_post "/lean/v1/$WS/window/clear" "{\"epoch\":$E,\"queued\":[]}")
  [ "$code" = "200" ] || { bad "window/clear at the current epoch returned $code"; return 1; }
  ok "current epoch admitted on the same verbs (the 403s are the check firing)"

  # A correct epoch still has to pass the CAS guard.
  code=$(gw_post "/lean/v1/$WS/manifest" \
    "{\"manifest\":{\"seq\":999,\"entries\":{}},\"expected_etag\":\"\\\"deadbeef\\\"\",\"epoch\":$E,\"flush_uuid\":\"x\"}")
  [ "$code" = "409" ] || { bad "manifest CAS with a bogus etag returned $code (want 409 cas-miss)"; return 1; }
  local seq_after
  seq_after=$(manif "$P" | jq -r '.seq')
  [ "$seq_after" != "999" ] || { bad "the bogus manifest LANDED"; return 1; }
  ok "current epoch + bad etag ⇒ 409 cas-miss, manifest untouched (seq $seq_after)"

  code=$($K -n flint-system exec curl -- sh -c \
    "curl -sS -o /dev/null -w '%{http_code}' '$GW/lean/v1/$WS/status'" 2>/dev/null)
  [ "$code" = "401" ] || { bad "unauthenticated status returned $code (want 401)"; return 1; }
  code=$(gw_get "/lean/v1/no-such-ws/status")
  [ "$code" = "404" ] || { bad "unknown workspace returned $code (want 404)"; return 1; }
  ok "no bearer ⇒ 401, unknown workspace ⇒ 404"
}

# ─────────────────────────────────────────────────────────────────────
# C7  The barrier window refuses HITL writes, and releases them again.
#     The window is availability/UX, not safety (LeanNoWindowHolds) —
#     this leg proves the UX contract is actually wired.
# ─────────────────────────────────────────────────────────────────────
c7_window_refusal() {
  local P=tenants/c7 R=/work/c7 WS=c7
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo seed > $R/seed.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }

  local E now code
  E=$(gw_epoch $WS); now=$(gw_now $WS)
  code=$(gw_post "/lean/v1/$WS/window/open" "{\"epoch\":$E,\"deadline_unix\":$((now + 120))}")
  [ "$code" = "200" ] || { bad "window/open returned $code"; return 1; }

  code=$(gw_put "/lean/v1/$WS/files/ui.txt" "during-window")
  [ "$code" = "409" ] || { bad "HITL write during an open window returned $code (want 409)"; return 1; }
  has "barrier-window-open" "$(gw_body)" || { bad "409 but not barrier-window-open: $(gw_body)"; return 1; }
  has "retry-after" "$(gw_hdr | tr 'A-Z' 'a-z')" || { bad "409 carries no Retry-After: $(gw_hdr)"; return 1; }
  ok "open window ⇒ 409 barrier-window-open + Retry-After"

  code=$(gw_post "/lean/v1/$WS/window/clear" "{\"epoch\":$E,\"queued\":[]}")
  [ "$code" = "200" ] || { bad "window/clear returned $code"; return 1; }
  # ANTI-VACUITY: the same write must succeed once the window is gone.
  code=$(gw_put "/lean/v1/$WS/files/ui.txt" "after-window")
  [ "$code" = "200" ] || { bad "HITL write after the window returned $code — the 409 was not the window"; return 1; }
  ok "window cleared ⇒ the same write is admitted"

  local out
  out=$(sy chaos-a $P $R barrier)
  has "consumed=1" "$out" || { bad "barrier did not consume the released write: $out"; return 1; }
  local body
  body=$(objcat "$P/files/ui.txt")
  [ "$body" = "after-window" ] || { bad "ui.txt is '$body'"; return 1; }
  manif "$P" | jq -e '.entries["ui.txt"]' > /dev/null || { bad "manifest does not cite ui.txt"; return 1; }
  ok "the released write was consumed and cited"
}

# ─────────────────────────────────────────────────────────────────────
# C8  Gateway outage.
#     Claim: the gateway is the CONTROL plane only — agents keep
#     publishing through an outage. The UI is what degrades.
# ─────────────────────────────────────────────────────────────────────
c8_gateway_outage() {
  local P=tenants/c8 R=/work/c8 WS=c8
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo base > $R/base.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }
  local code
  code=$(gw_put "/lean/v1/$WS/files/before.txt" "before-outage")
  [ "$code" = "200" ] || { bad "pre-outage HITL write returned $code"; return 1; }
  ok "pre-outage: HITL write accepted"

  $K -n flint-system scale deploy/lean-gateway --replicas=0 > /dev/null
  $K -n flint-system wait --for=delete pod -l app=lean-gateway --timeout=120s > /dev/null 2>&1
  code=$(gw_put "/lean/v1/$WS/files/during.txt" "during-outage")
  [ "$code" != "200" ] || { bad "HITL write SUCCEEDED with the gateway scaled to zero — leg vacuous"; return 1; }
  ok "outage confirmed: HITL write returns '$code' (no gateway)"

  # The claim: the data plane does not depend on the control plane.
  inpod chaos-a "echo written-during-outage > $R/outage.txt; echo w" > /dev/null
  local out
  out=$(sy chaos-a $P $R barrier)
  has "up=1" "$out" || { bad "the sidecar could not publish during the outage: $out"; return 1; }
  local body
  body=$(objcat "$P/files/outage.txt")
  [ "$body" = "written-during-outage" ] || { bad "outage publish body is '$body'"; return 1; }
  manif "$P" | jq -e '.entries["outage.txt"]' > /dev/null || { bad "outage publish not cited"; return 1; }
  ok "the sidecar published and cited a new file WHILE the gateway was down"
  note "a GATEWAY outage costs only the fourth of plan §2.2's four stated effects: publishing, checkout and sync are untouched. The shipped flint-sync writes the manifest/window/inbox cells DIRECTLY to the store (barrier.rs:257,261,384,467) and links no HTTP client at all, so the gateway is not on its write path. C12 drills the other half — the proxy — and gets all four. Gateway and proxy are separate failure domains; plan §2.2 states them as one line ('gateway/proxy down'), and Phase 3's 'assert ALL FOUR effects' is a PROXY criterion."

  $K -n flint-system scale deploy/lean-gateway --replicas=1 > /dev/null
  $K -n flint-system rollout status deploy/lean-gateway --timeout=180s > /dev/null
  # rollout status returns on Deployment availability; the Service
  # endpoint can still be a beat behind. Poll the door itself.
  gw_healthy 60 || { bad "gateway never answered /healthz after scale-up"; return 1; }
  code=$(gw_put "/lean/v1/$WS/files/after.txt" "after-outage")
  [ "$code" = "200" ] || { bad "post-recovery HITL write returned $code"; return 1; }
  ok "gateway recovered: HITL writes accepted again (stateless, nothing to rebuild)"
}

# ─────────────────────────────────────────────────────────────────────
# C9  The state-directory occupancy lock.
#     The 0b rig found this one the hard way: without it a second
#     flint-sync self-recognizes the lease, deposes a LIVE sibling, and
#     both write the tree.
# ─────────────────────────────────────────────────────────────────────
c9_occupancy_lock() {
  local P=tenants/c9 R=/work/c9 N=3000
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  mkfiles chaos-a $R $N g > /dev/null
  sy_bg chaos-a $P $R /work/c9.log barrier
  # The contended window has to be OBSERVED open, not assumed: the
  # second sidecar must start while the first is demonstrably mid-barrier.
  wait_key "$P" "g0200.txt" 150 || { bad "the first barrier never got going"; return 1; }
  local second rc
  second=$(sy chaos-a $P $R barrier); rc=$?
  [ "$rc" -ne 0 ] || { bad "a SECOND flint-sync over one tree exited 0 — the occupancy lock is gone"; return 1; }
  has "another flint-sync already holds this workspace" "$second" \
    || { bad "second run failed for the wrong reason: $second"; return 1; }
  ok "second sidecar over one tree refused: $(printf '%s' "$second" | grep 'another flint-sync' | head -1)"

  await_exit chaos-a flint-sync 90 || { bad "the first barrier never exited"; return 1; }
  # ANTI-VACUITY: the lock must RELEASE, or this leg would also pass on
  # a sidecar that can never run twice at all.
  local third
  third=$(sy chaos-a $P $R barrier) || { bad "the lock never released: $third"; return 1; }
  ok "lock released after the first exited: $(printf '%s' "$third" | tail -1)"
}

# ─────────────────────────────────────────────────────────────────────
# C10 Prefix containment in a shared bucket.
#     A proxy hands lean ONE subtree. The GC must never reach outside
#     `<prefix>/files/` — including into a neighbour whose key is a
#     STRING prefix match (tenants/c10 vs tenants/c10-sibling).
# ─────────────────────────────────────────────────────────────────────
c10_prefix_containment() {
  local P=tenants/c10 R=/work/c10
  putobj "$P-sibling/files/keep.txt" "sibling-must-survive"
  putobj "$P/other/keep.txt" "same-prefix-outside-files"
  putobj "$P/files-extra/keep.txt" "adjacent-must-survive"
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo x > $R/x.txt; echo y > $R/y.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }

  inpod chaos-a "rm -f $R/y.txt; echo rm" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "barrier 1"; return 1; }
  sy chaos-a $P $R barrier > /dev/null || { bad "barrier 2"; return 1; }
  # ANTI-VACUITY: the GC must actually have deleted something in this
  # run, or "the neighbours survived" is a statement about a no-op.
  if objexists "$P/files/y.txt"; then bad "the GC deleted nothing — containment leg is vacuous"; return 1; fi
  ok "the GC ran and removed y.txt"

  objcat "$P-sibling/files/keep.txt" | grep -c 'sibling-must-survive' > /dev/null \
    || { bad "the string-prefix NEIGHBOUR tenants/c10-sibling was swept"; return 1; }
  objcat "$P/other/keep.txt" | grep -c 'same-prefix-outside-files' > /dev/null \
    || { bad "a key under our own prefix but outside files/ was swept"; return 1; }
  objcat "$P/files-extra/keep.txt" | grep -c 'adjacent-must-survive' > /dev/null \
    || { bad "the adjacent files-extra/ prefix was swept"; return 1; }
  ok "all three neighbours survived (sibling prefix, non-files subtree, adjacent prefix)"
}

# ─────────────────────────────────────────────────────────────────────
# C11 A HITL upload survives later barriers with no sync verb involved.
#     This is the model's load-bearing finding made physical: merge
#     alone preserves a foreign entry for exactly ONE barrier, because
#     Finish absorbs it into the merge base and a later local delete
#     then destroys the citation. The inbox's consume path is what
#     makes it durable — so the leg runs barriers that do unrelated
#     work, INCLUDING a GC, and demands the human's file is still there.
# ─────────────────────────────────────────────────────────────────────
c11_hitl_survives_barriers() {
  local P=tenants/c11 R=/work/c11 WS=c11
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo agent-a > $R/a.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }

  local code
  code=$(gw_put "/lean/v1/$WS/files/human.txt" "human-uploaded-bytes")
  [ "$code" = "200" ] || { bad "HITL upload returned $code"; return 1; }
  ok "human uploaded human.txt (object + inbox entry only — no manifest edit)"

  # Barrier 1 consumes it. The agent is doing unrelated work throughout;
  # the sync verb is never invoked.
  local out
  out=$(sy chaos-a $P $R barrier)
  has "consumed=1" "$out" || { bad "barrier 1 did not consume the upload: $out"; return 1; }
  local local_copy
  local_copy=$(inpod chaos-a "cat $R/human.txt")
  [ "$local_copy" = "human-uploaded-bytes" ] || { bad "consume did not adopt into the tree: '$local_copy'"; return 1; }
  manif "$P" | jq -e '.entries["human.txt"]' > /dev/null \
    || { bad "barrier 1 did not cite human.txt — amputated at the first barrier"; return 1; }
  ok "barrier 1: consumed, adopted into the tree, cited"

  # Barrier 2: unrelated add.
  inpod chaos-a "echo agent-b > $R/b.txt; echo w" > /dev/null
  out=$(sy chaos-a $P $R barrier)
  has "up=1" "$out" || { bad "barrier 2 published nothing — leg would be vacuous: $out"; return 1; }
  manif "$P" | jq -e '.entries["human.txt"]' > /dev/null \
    || { bad "human.txt lost at barrier 2 (absorbed into the merge base, then dropped)"; return 1; }

  # Barriers 3 and 4: an unrelated DELETE, which is the exact operation
  # that destroyed the citation in the model's depth-12 counterexample.
  inpod chaos-a "rm -f $R/a.txt; echo rm" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "barrier 3"; return 1; }
  sy chaos-a $P $R barrier > /dev/null || { bad "barrier 4"; return 1; }
  if objexists "$P/files/a.txt"; then bad "the GC never ran — the delete leg is vacuous"; return 1; fi
  ok "barriers 2-4 did unrelated work including a GC"

  local body cited
  body=$(objcat "$P/files/human.txt")
  [ "$body" = "human-uploaded-bytes" ] || { bad "human.txt bytes are now '$body'"; return 1; }
  cited=$(manif "$P" | jq -r '.entries|keys[]' | tr '\n' ' ')
  has "human.txt" "$cited" || { bad "human.txt un-cited after four barriers (cited: $cited)"; return 1; }
  ok "human.txt intact and cited after 4 barriers and a GC, with no sync verb — cited set: $cited"
}

# ─────────────────────────────────────────────────────────────────────
# C12 Proxy unreachable — the OTHER failure domain.
#     Plan §2.2 states one failure mode for "gateway/proxy down" with
#     four effects. C8 shows the gateway costs one of them; this leg
#     shows the proxy costs all four. The sharp assertion is the third:
#     a wedged checkout must FAIL, never write the agent-start marker
#     over an empty tree — a wedge that looks like an empty workspace
#     would start the agent on nothing.
# ─────────────────────────────────────────────────────────────────────
c12_proxy_outage() {
  local P=tenants/c12 R=/work/c12 R2=/work/c12-fresh DEAD=http://127.0.0.1:1
  sy chaos-a $P $R checkout > /dev/null || { bad "checkout"; return 1; }
  inpod chaos-a "mkdir -p $R; echo base > $R/base.txt; echo w" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "base barrier"; return 1; }
  ok "published base.txt with the proxy reachable"

  inpod chaos-a "echo unpublished > $R/pending.txt; echo w" > /dev/null
  local out rc
  out=$(sy_ep chaos-a $P $R $DEAD barrier); rc=$?
  [ "$rc" -ne 0 ] || { bad "the barrier SUCCEEDED with the proxy unreachable: $out"; return 1; }
  ok "publish FAILS with the proxy unreachable (exit $rc)"

  # The agent's own work must survive its sidecar failing to publish.
  local body
  body=$(inpod chaos-a "cat $R/pending.txt")
  [ "$body" = "unpublished" ] || { bad "the failed barrier damaged the local tree: '$body'"; return 1; }
  ok "the local tree is untouched — a failed publish is not a lost edit"

  out=$(sy_ep chaos-a $P $R2 $DEAD checkout); rc=$?
  [ "$rc" -ne 0 ] || { bad "checkout SUCCEEDED with the proxy unreachable: $out"; return 1; }
  local marker
  marker=$(inpod chaos-a "test -f $R2/.flint-sync/checkout-complete && echo yes || echo no")
  [ "$marker" = "no" ] || { bad "the checkout gate marker was written over an EMPTY tree — a wedge that looks like an empty workspace"; return 1; }
  ok "checkout WEDGES and the agent-start gate stays shut (no marker over an empty tree)"

  # ANTI-VACUITY: every refusal above must become a success once the
  # proxy answers again, or the leg proved only that the paths are broken.
  out=$(sy chaos-a $P $R barrier)
  has "up=1" "$out" || { bad "publish did not recover once the proxy answered: $out"; return 1; }
  out=$(sy chaos-a $P $R2 checkout)
  has "materialized" "$out" || { bad "checkout did not recover: $out"; return 1; }
  local tree
  tree=$(inpod chaos-a "cd $R2 && ls | sort | tr '\n' ' '")
  [ "$tree" = "base.txt pending.txt " ] || { bad "recovered checkout tree is '$tree'"; return 1; }
  ok "both recover with the proxy back — publish and checkout, same subtree"
  note "the four effects in plan §2.2 are PROXY effects, not gateway effects: proxy unreachable ⇒ publish fails, checkout wedges, sync dead, HITL fails; gateway down (C8) ⇒ only HITL fails. Worth splitting in the plan, and it locates P5 enforcement at the PROXY — where the sidecar's writes actually go, and where the epoch is already on the wire in GenerationStamps."
}

# ─────────────────────────────────────────────────────────────────────
echo "flint-lean chaos drill — context $CTX"

# Preflight. The drill must be RE-RUNNABLE: a crash leg leaves orphan
# objects and a held lease behind by design, and a second run over that
# debris would be measuring the previous run. So both halves of the
# state — the bucket subtree and every emptyDir — are reset first.
echo "  … resetting the rig (fresh emptyDirs + empty subtree)"
$K delete pod chaos-a chaos-k chaos-k2 chaos-s chaos-s2 \
  --ignore-not-found --wait=true --timeout=120s > /dev/null 2>&1
$K apply -f chaos.yaml > /dev/null 2>&1
$K -n flint-system wait --for=condition=Ready pod/mc pod/curl --timeout=180s > /dev/null 2>&1 \
  || { echo "FAIL: oracle pods (mc/curl) not Ready"; exit 1; }
$K wait --for=condition=Ready pod/chaos-a pod/chaos-k pod/chaos-k2 pod/chaos-s pod/chaos-s2 \
  --timeout=180s > /dev/null 2>&1 || { echo "FAIL: chaos pods not Ready"; exit 1; }
mcx mc alias set m http://minio.flint-system.svc:9000 drill drillsecret > /dev/null \
  || { echo "FAIL: mc alias"; exit 1; }
mcx mc rm --recursive --force "m/$BUCKET/tenants" > /dev/null 2>&1
$K -n flint-system scale deploy/lean-gateway --replicas=1 > /dev/null 2>&1
$K -n flint-system rollout status deploy/lean-gateway --timeout=180s > /dev/null \
  || { echo "FAIL: gateway not rolled out"; exit 1; }
gw_healthy 60 || { echo "FAIL: gateway does not answer /healthz"; exit 1; }

leg "C1  crash mid-barrier, recover over the same emptyDir" c1_crash_midbarrier
leg "C2  pod loss mid-barrier, fresh-pod takeover"          c2_podloss_takeover
leg "C3  deposed straggler resumes"                          c3_straggler_after_takeover
leg "C4  container restart + two-scan delete"                c4_restart_and_two_scan_delete
leg "C5  HITL write vs dirty local file"                     c5_hitl_conflict
leg "C6  per-request epoch validation (P5)"                  c6_gateway_epoch_validation
leg "C7  barrier window refuses HITL"                        c7_window_refusal
leg "C8  gateway outage"                                     c8_gateway_outage
leg "C9  state-dir occupancy lock"                           c9_occupancy_lock
leg "C10 prefix containment"                                 c10_prefix_containment
leg "C11 HITL upload survives later barriers (no sync)"      c11_hitl_survives_barriers
leg "C12 proxy unreachable (the other failure domain)"       c12_proxy_outage

echo
echo "════════════════════════════════════════════════"
echo "flint-lean chaos drill: $PASS passed, $FAILED failed (of 12)"
if [ ${#NOTES[@]} -gt 0 ]; then
  echo
  echo "Measured residuals:"
  for n in "${NOTES[@]}"; do echo "  · $n"; done
fi
[ "$FAILED" -eq 0 ]
