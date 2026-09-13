#!/usr/bin/env bash
# NOT RETIRED — re-checked 2026-09-03 against the CSI cutover (S12).
# An earlier banner here declared this path retired. That was WRONG, and
# a wrong banner is the worse failure: a suite nobody runs because it
# says not to. This rig never used the lean webhook. It creates no
# FlintLeanWorkspace, reads no chert.us/lean-workspace label, and needs
# no operator: its pods are hand-authored in verbs.yaml (verbs-a, verbs-b, verbs-old, …) with an
# EXPLICIT `sync` container, and the drill execs flint-sync verbs into
# them with per-leg env and per-leg subtrees.
#
# What the CSI cutover changed is DELIVERY — how a workspace reaches a
# pod — which this suite does not test and never did. Delivery is
# drilled by s3csi/e2e/run-s3csi.sh (S11, S12, S13) and, across
# clusters, s3csi/e2e/multi/run-multi.sh (M3). What B1-B25 tests is the
# bucket PROTOCOL, and delivery does not change it.
#
# So design §10.2 S12's "re-target every `kubectl exec -c flint-sync`
# step at the worker pod in flint-workers" does not apply here: there is
# no injected container to re-target, and a worker pod could not host
# these legs anyway — one CSI volume is one prefix, while every leg here
# needs its own subtree, and reset_pods kills the resident syncer, which
# under CSI would take PID 1 of the worker with it.
# The BOUNDARY-VERBS bucket drill (plan §6, legs B1-B25 + B12b) —
# against a real MinIO, through the real S3 backend, with the oracle
# reading the bucket directly.
#
# The unit battery (110 tests) and the formal gate (63 runs) prove the
# protocol. This drill proves the things neither can see: real crash
# timing, a real proxy that strips a header, real request counts, real
# object versions, and the two-consecutive-scans rule against a real
# filesystem clock.
#
# THE FALSIFIABILITY FIXTURE is the 1-hour floor. With
# FLINT_SYNC_FLOOR_SECS=3600 the cadence cannot advance the manifest, so
# any advance is attributable to the mechanism under test. Legs that
# need the ordinary floor say so and set their own.
#
# EVERY leg carries an anti-vacuity guard: a leg that cannot observe its
# own precondition FAILS rather than passing quietly. The lite drill's
# lesson was that 24 of 41 proposed legs would have PASSED IF BROKEN.
#
# A GUARD IS ONLY AS GOOD AS THE FIELD IT READS. Writing B12b turned up
# two guards in this file that could not fail:
#   - `mc ls --versions --json` emits NO `isLatest` field (its keys are
#     etag, key, lastModified, size, status, storageClass, type, url,
#     versionId, versionOrdinal). `vers()` read `.isLatest // false`, so
#     `latest` was false for every row and B23's "the cited version is
#     now noncurrent" check asserted a constant. `versionOrdinal` is the
#     real signal — highest is current.
#   - `jq -e` sets its exit status from the LAST output, so a bare
#     `select` over a stream exits 4 ("no output") whenever the final
#     row does not match, even when an earlier one did.
# Same family as B8's "minio/mc has no grep": the tool did not have the
# thing the oracle assumed. Prefer probes you have watched go red.
#
# Prereqs: kind cluster `flint-lean-verbs` with flint-sync:e2e and
# flint-lean-gateway:e2e loaded; minio.yaml + verbs.yaml applied; bucket
# any bucket; versioning is not required.
#
#   kind create cluster --name flint-lean-verbs
#   cd lean/syncer && cargo zigbuild --release --features s3 \
#       --target aarch64-unknown-linux-musl
#   cd ../gateway && cargo zigbuild --release --features s3 \
#       --target aarch64-unknown-linux-musl
#   cd ../e2e && cp ../syncer/target/aarch64-unknown-linux-musl/release/flint-sync \
#       ../gateway/target/aarch64-unknown-linux-musl/release/flint-lean-gateway .
#   docker build -t flint-sync:e2e         -f Dockerfile.syncer .
#   docker build -t flint-lean-gateway:e2e -f Dockerfile.gateway .
#   kind load docker-image flint-sync:e2e flint-lean-gateway:e2e \
#       --name flint-lean-verbs
#   kubectl apply -f minio.yaml -f verbs.yaml
#   ./run-verbs.sh            # ONLY=B9 ./run-verbs.sh runs one leg
#
# B14 needs one more image the repo does not build by default: a
# GENUINELY pre-D0 binary, so the mixed-fleet hazard is demonstrated
# rather than simulated. Build it from the commit before the boundary
# verbs existed, in a worktree so the working tree is untouched:
#
#   git worktree add /tmp/predo 69b35978
#   cd /tmp/predo/lean/syncer && cargo zigbuild --release --features s3 \
#       --target aarch64-unknown-linux-musl
#   # then docker build that binary as flint-sync:predo with
#   # Dockerfile.syncer and kind load it.
#
# Without it B14 FAILS loudly rather than skipping: a mixed-fleet leg
# with no old binary in it is not a mixed-fleet leg.
#
# NEVER EDIT THIS FILE WHILE IT IS RUNNING. bash reads a script
# incrementally, so a mid-run edit shifts the offsets underneath it —
# one run re-executed half a leg and then died on `ence_fires: command
# not found`, and every failure it reported in between was an artifact.
set -u
cd "$(dirname "$0")"

CTX=${CTX:-kind-flint-lean-verbs}
K="kubectl --context $CTX"
BUCKET=agentws
GW=http://lean-gateway.flint-system.svc:8091
TOK=verbs-drill-token-0123456789
ONLY=${ONLY:-}
# Which container the pod helpers exec into. B14 flips it to reach the
# pre-D0 binary and the shipping one over one shared workspace.
CONT=sync

PASS=0
FAILED=0
SKIPPED=0
NOTES=()

# U32: the drill's own accounting. Until now the roster existed only as
# a column of `leg` calls, so deleting one produced "27 passed, 0
# failed" — green, and one claim lighter. Two legs (B14, B25) were
# additionally assigned to NO phase gate in the plan, which meant the
# only empirical check of the work-metered budget and the only
# mixed-fleet leg could both be skipped while every phase read green.
#
# This is the register §6 asks for, as an assertion rather than a
# comment: every id here must run, and every leg that runs must be
# here. Adding a leg without adding its id fails the drill, which is
# the forcing function that keeps the plan's matrix honest.
EXPECTED_LEGS="B1 B2 B3 B4 B5 B6 B7 B8 B9 B10 B11a B11b B11c B12 B12b B13 B14 \
B15 B16 B17 B18 B19 B20 B21 B22 B23 B24 B25"
RAN_LEGS=""

note() { NOTES+=("$1"); echo "  note: $1"; }
ok()   { echo "  ok: $1"; }
bad()  { echo "  BAD: $1"; }
has()  { [ "$(printf '%s' "$2" | grep -c -- "$1")" -gt 0 ]; }
# The ack is pretty-printed JSON, so field checks go through jq —
# grepping '"status":"ok"' matches nothing and reads like a failure.
ajq()  { printf '%s' "$1" | jq -r "$2" 2>/dev/null; }

# Every leg starts from a pod with no syncer running. A leg that FAILS
# skips its own cleanup, and the loop it leaves behind renews a lease —
# so the next leg claims, deposes it, and reads a refused-fenced ack it
# never asked for. That is a harness bug producing a product-shaped
# symptom, which is the most expensive kind.
reset_pods() {
  local p
  for p in verbs-a verbs-b verbs-s verbs-s2; do
    $K exec "$p" -c sync -- /bin/sh -c 'kill -9 $(pidof flint-sync) 2>/dev/null; true' > /dev/null 2>&1
  done
  for p in verbs-a verbs-b verbs-s verbs-s2; do
    await_exit "$p" flint-sync 10 > /dev/null || note "$p still has a syncer process at leg start"
  done
}

leg() {
  local name=$1; shift
  RAN_LEGS="$RAN_LEGS ${name%% *}"
  if [ -n "$ONLY" ] && [ "${name%% *}" != "$ONLY" ]; then return 0; fi
  echo
  echo "── $name"
  reset_pods
  if "$@"; then
    PASS=$((PASS + 1)); echo "  PASS"
  else
    FAILED=$((FAILED + 1)); echo "  FAIL"
  fi
}

# ── rig helpers ──────────────────────────────────────────────────────
# flint-sync in <pod> with the leg's own prefix/root and extra env.
# Legs never share a subtree: one leg's debris must not become the
# next leg's starting state.
# A one-shot verb claims the lease FIRST, and `claim` loops forever
# while another syncer holds it — that is the design, not a bug. A leg
# that gets the ownership wrong must FAIL, not hang the drill, so every
# one-shot carries a timeout and the caller reads exit 124 as "blocked".
sy() { # <pod> <prefix> <root> <extra-env> <args…>
  local pod=$1 prefix=$2 root=$3 envs=$4; shift 4
  $K exec "$pod" -c "$CONT" -- /bin/sh -c \
    "timeout ${SY_TIMEOUT:-180} env FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root $envs \
     /usr/local/bin/flint-sync $*" 2>&1
}
# Detached: the process must outlive the exec session so the script can
# storm it, stop it, or kill it mid-flight.
sy_bg() { # <pod> <prefix> <root> <extra-env> <log> <args…>
  local pod=$1 prefix=$2 root=$3 envs=$4 log=$5; shift 5
  $K exec "$pod" -c "$CONT" -- /bin/sh -c \
    "nohup env FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root $envs \
     /usr/local/bin/flint-sync $* > $log 2>&1 & echo bg-started" > /dev/null 2>&1
}
inpod() { local pod=$1; shift; $K exec "$pod" -c "$CONT" -- /bin/sh -c "$*" 2>/dev/null; }

# `pidof` alone is the WRONG liveness test: PID 1 here is `sleep
# infinity`, which never reaps, so an exited flint-sync sits as a
# ZOMBIE and pidof finds it forever. Read the state field.
proc_alive() { # <pod> <name>
  inpod "$1" "for p in \$(pidof $2 2>/dev/null); do \
                s=\$(awk '{print \$3}' /proc/\$p/stat 2>/dev/null); \
                [ \"\$s\" != Z ] && exit 0; done; exit 1"
}
await_exit() { # <pod> <name> <iters>
  local i
  for i in $(seq 1 "$3"); do proc_alive "$1" "$2" || return 0; sleep 2; done
  return 1
}
killsync() { inpod "$1" "kill -9 \$(pidof flint-sync) 2>/dev/null; true" > /dev/null; }
stopsync() { inpod "$1" "kill -STOP \$(pidof flint-sync) 2>/dev/null; true" > /dev/null; }
contsync() { inpod "$1" "kill -CONT \$(pidof flint-sync) 2>/dev/null; true" > /dev/null; }
termsync() { inpod "$1" "kill -TERM \$(pidof flint-sync) 2>/dev/null; true" > /dev/null; }

# Zero-padded names: the upload set is a BTreeSet walked at fan-out 1,
# so uploads happen in lexicographic order — which with padding is also
# numeric order, turning "did the kill land inside the upload loop?"
# into two cheap HEADs instead of a race against a sleep.
mkfiles() { # <pod> <dir> <n> <tag>
  inpod "$1" "mkdir -p $2 && awk -v d=$2 -v n=$3 -v t=$4 \
    \"BEGIN{for(i=1;i<=n;i++){fn=sprintf(\\\"%s/%s%04d.txt\\\", d, t, i); print t \\\"-\\\" i > fn; close(fn)}}\" && echo made"
}
pad() { printf '%04d' "$1"; }

# ── the oracle: reads the bucket DIRECTLY, never through the syncer's
#    own code, or one bug could hide another ─────────────────────────
mcx()      { $K -n flint-system exec mc -- "$@" 2>/dev/null; }
objcat()   { mcx mc cat "m/$BUCKET/$1"; }
objexists(){ mcx mc stat "m/$BUCKET/$1" > /dev/null 2>&1; }
objstat()  { mcx mc stat --json "m/$BUCKET/$1"; }
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
# The FENCING seq is the POINTER's: a takeover rotation bumps it and
# leaves `entries_seq` alone, which is the point of the layout.
mseq()     { local c m; c=$(objcat "$1/.flint/lean/current"); [ -n "$c" ] && { printf '%s' "$c" | jq -r '.seq // 0'; return; }; m=$(manif "$1"); [ -z "$m" ] && { echo 0; return; }; printf '%s' "$m" | jq -r '.seq // 0'; }
# Every version of one key: {v, latest, size}.
#
# `mc ls --versions --json` EMITS NO `isLatest` FIELD. Its keys are
# etag, key, lastModified, size, status, storageClass, type, url,
# versionId, versionOrdinal — so the old `(.isLatest // false)` was
# false for EVERY row, and any oracle reading it was reading a
# constant. B23's "the cited version is now noncurrent" guard was
# asserting `latest == "false"` against that constant and therefore
# passed whatever the bucket said. Same family as B8's `minio/mc` has
# no grep: the tool does not have the field the oracle assumed.
#
# `versionOrdinal` is the real signal — highest is current.
vers()     { mcx mc ls --versions --json "m/$BUCKET/$1" \
               | jq -s -c 'map(select(.versionId))
                           | (map(.versionOrdinal // 0) | max) as $top
                           | .[] | {v:.versionId,
                                    latest:((.versionOrdinal // 0) == $top),
                                    size:.size}'; }
vcount()   { vers "$1" | grep -c . ; }
vcat()     { mcx mc cat --version-id "$2" "m/$BUCKET/$1"; }
# Is version $2 of key $1 still present? (mc has no grep/awk/sed, so the
# filtering is done HERE, on the host — the B8 lesson.)
# NB `jq -e` sets its status from the LAST output, so a bare `select`
# over a stream exits 4 ("no output") whenever the last row does not
# match — even when an earlier one did. Slurp and use `any`.
vhas()     { vers "$1" | jq -s -e --arg v "$2" 'any(.[]; .v==$v)' > /dev/null 2>&1; }
# Is version $2 of key $1 present AND NOT current? That is the old
# reaper rule's kill zone: neither `keep` nor `is_current`.
vnoncurrent() { vers "$1" | jq -s -e --arg v "$2" 'any(.[]; .v==$v and .latest==false)' > /dev/null 2>&1; }
# The manifest's boundary provenance, from the OBJECT's metadata — an
# operator has to be able to read it from the bucket alone.
# The POINTER carries the stamp now: `cas_write_stamped` passes the same
# `GenerationStamps` to the generation PUT and to the pointer CAS, so the
# provenance is still one HEAD away — off a 191 B object rather than a
# manifest that can run to hundreds of MiB.
bsource()  { objstat "$1/.flint/lean/current" | jq -r '.metadata["X-Amz-Meta-Flint-Boundary-Source"] // .metadata["x-amz-meta-flint-boundary-source"] // empty'; }

wait_key() { # <prefix> <relative key> <iters>
  local i
  for i in $(seq 1 "$3"); do objexists "$1/files/$2" && return 0; sleep 0.2; done
  return 1
}
wait_seq_gt() { # <prefix> <seq> <iters>
  local i s
  for i in $(seq 1 "$3"); do
    s=$(mseq "$1"); [ -n "$s" ] && [ "$s" -gt "$2" ] && return 0
    sleep 1
  done
  return 1
}

gw_get()  { $K -n flint-system exec curl -- sh -c \
              "curl -sS -o /tmp/out -w '%{http_code}' -H 'Authorization: Bearer $TOK' '$GW$1'" 2>/dev/null; }
gw_put()  { printf '%s' "$2" | $K -n flint-system exec -i curl -- sh -c \
              "cat > /tmp/body && curl -sS -o /tmp/out -w '%{http_code}' -X PUT \
               -H 'Authorization: Bearer $TOK' --data-binary @/tmp/body '$GW$1'" 2>/dev/null; }
gw_body() { $K -n flint-system exec curl -- cat /tmp/out 2>/dev/null; }
gw_healthy() {
  local i c
  for i in $(seq 1 "$1"); do
    c=$($K -n flint-system exec curl -- sh -c "curl -sS -o /dev/null -w '%{http_code}' '$GW/healthz'" 2>/dev/null)
    [ "$c" = "200" ] && return 0
    sleep 1
  done
  return 1
}

# Ack/marker readers (the AGENT's side of the file protocol).
ackf()  { inpod "$1" "cat $2/.flint/$3.ack 2>/dev/null"; }
caps()  { inpod "$1" "cat $2/.flint/capabilities.json 2>/dev/null"; }
ticker(){ inpod "$1" "cat $2/.flint/remote.seq 2>/dev/null"; }
gauges(){ inpod "$1" "cat $2/.flint-sync/gauges.json 2>/dev/null"; }
touchp(){ inpod "$1" "mkdir -p $2/.flint && printf '%s' '$4' > $2/.flint/$3.tmp && mv $2/.flint/$3.tmp $2/.flint/$3"; }
# Wait for an ack whose body matches a pattern (never sleep-and-hope).
wait_ack() { # <pod> <root> <verb> <pattern> <iters>
  local i a
  for i in $(seq 1 "$5"); do
    a=$(ackf "$1" "$2" "$3")
    has "$4" "$a" && { printf '%s' "$a"; return 0; }
    sleep 1
  done
  printf '%s' "$a"
  return 1
}

# `flint-sync status` — the operator's read of a LIVE workspace. It
# takes no lease and no occupancy lock, which is exactly why a leg may
# call it while a run loop is up.
statusj() { # <pod> <prefix> <root> <jq-filter>
  local out
  out=$(sy "$1" "$2" "$3" "" status)
  printf '%s' "$out" | jq -r "$4" 2>/dev/null
}

# The epoch cell, read from the bucket: holder, epoch and the D12 lease
# echo. The oracle for "was there a takeover?".
epochdoc() { objcat "$1/.flint/lean/epoch"; }
# The renewal clock is NOT in the body. A8 judges takeover against the
# STORE's clock, so `last_renew_unix` is the epoch object's
# Last-Modified — read it from the object's metadata or the leg measures
# a field that is always null.
epoch_mtime() { objstat "$1/.flint/lean/epoch" | jq -r '.lastModified // empty'; }

# A foreign takeover is NOT instant and must not be waited on with a
# sleep: `claim` needs QUIET_POLLS=6 observations of an unchanging cell
# at 10 s each, so a successor claims ~60 s after its predecessor stops
# renewing. Legs that depose a syncer pay this, once, on purpose.
TAKEOVER_SECS=150
await_file() { # <pod> <path> <secs>
  local i
  for i in $(seq 1 $(( $3 / 3 ))); do
    inpod "$1" "test -e $2" && return 0
    sleep 3
  done
  return 1
}

# A tree hash the leg can compare across an event — B18 and B35 both
# turn "did the syncer mutate anything?" into one string. The control
# namespace is excluded: the syncer writes its own marker files there
# and that is not a mutation of the AGENT's tree.
treehash() { # <pod> <root>
  inpod "$1" "cd $2 2>/dev/null && find . -type f \
    ! -path './.flint/*' ! -path './.flint-sync/*' -exec md5sum {} + 2>/dev/null | sort | md5sum"
}


# The gateway is the HITL party: it holds no lease and never edits a
# manifest, which is precisely why it can write while a syncer runs.
gw_put_ws() { gw_put "/lean/v1/$1/files/$2" "$3"; }
gw_get_ws() { gw_get "/lean/v1/$1/files/$2"; }

# Wait until a citation installs a manifest whose boundary provenance is
# readable from the OBJECT's metadata alone — an operator has to be able
# to answer "which coherent point fired?" from the bucket.
wait_bsource() { # <prefix> <expected> <iters>
  local i b
  for i in $(seq 1 "$3"); do
    b=$(bsource "$1"); [ "$b" = "$2" ] && return 0
    sleep 1
  done
  printf '%s' "$b"
  return 1
}

# ─────────────────────────────────────────────────────────────────────
# B1  The sentinel publishes when cadence CANNOT.
#     The control runs first and is the whole point of the 1-hour
#     floor: if the manifest advances with no sentinel, this rig is not
#     measuring the sentinel.
# ─────────────────────────────────────────────────────────────────────
b1_sentinel_beats_cadence() {
  local P=tenants/b01 R=/work/b01
  inpod verbs-a "mkdir -p $R" > /dev/null
  sy_bg verbs-a $P $R "" /tmp/b01.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  inpod verbs-a "test -f $R/.flint-sync/checkout-complete" || { bad "checkout never completed"; return 1; }
  mkfiles verbs-a $R 5 f > /dev/null
  ok "5 dirty files, floor=3600"

  # CONTROL: cadence is out of the picture.
  local s0 s1
  s0=$(mseq $P)
  sleep 25
  s1=$(mseq $P)
  [ "$s0" = "$s1" ] || { bad "the manifest advanced from $s0 to $s1 with NO sentinel — the floor fixture is not holding"; return 1; }
  ok "control: 25 s with no sentinel, manifest still at seq $s0"

  touchp verbs-a $R publish '{"nonce":"b1-n1"}'
  local a
  a=$(wait_ack verbs-a $R publish 'b1-n1' 40) || { bad "no ack covering the nonce: $a"; return 1; }
  [ "$(ajq "$a" .status)" = "ok" ] || { bad "ack status is $(ajq "$a" .status), not ok"; return 1; }
  s1=$(mseq $P)
  [ "$s1" -gt "$s0" ] || { bad "the ack landed but the manifest did not advance ($s0 -> $s1)"; return 1; }
  local cited
  cited=$(manif $P | jq -r '.entries|keys|length')
  [ "$cited" = "5" ] || { bad "the boundary cites $cited files, not 5"; return 1; }
  ok "sentinel published: seq $s0 -> $s1, 5 files cited, ack covers b1-n1"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B2  Crash between consume and ack converges via the uniform re-run
#     rule. The ack the agent finally reads must name the RE-RUN's
#     install, never the pre-crash baseline.
# ─────────────────────────────────────────────────────────────────────
b2_crash_between_consume_and_ack() {
  local P=tenants/b02 R=/work/b02
  inpod verbs-a "mkdir -p $R" > /dev/null
  # Seed a first boundary so there IS a pre-crash baseline seq to
  # mistake for the answer.
  mkfiles verbs-a $R 2 seed > /dev/null
  sy verbs-a $P $R "" barrier > /dev/null
  local base
  base=$(mseq $P)
  [ "$base" -ge 1 ] || { bad "the seed barrier did not install"; return 1; }
  ok "seeded baseline at seq $base"

  mkfiles verbs-a $R 400 g > /dev/null
  sy_bg verbs-a $P $R "" /tmp/b02.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  touchp verbs-a $R publish '{"nonce":"b2-n1"}'

  # Land the kill INSIDE the honor: wait until an early key exists and
  # the last one does not. Two HEADs, no race against a sleep.
  wait_key $P "g0002.txt" 200 || { bad "the honor never started uploading"; return 1; }
  objexists "$P/files/g0400.txt" && { bad "the upload finished before the kill — nothing mid-flight to test"; return 1; }
  local pend
  pend=$(inpod verbs-a "ls $R/.flint-sync/publish.pending.json 2>/dev/null")
  [ -n "$pend" ] || { bad "no pending record at kill time — the sentinel was not consumed"; return 1; }
  inpod verbs-a "test -f $R/.flint/publish.ack" && { bad "an ack existed BEFORE the crash"; return 1; }
  killsync verbs-a
  ok "killed mid-honor: pending record present, ack absent, uploads incomplete"

  # Same emptyDir: the restart must settle the pending sentinel before
  # it may consume a fresh one.
  sy_bg verbs-a $P $R "" /tmp/b02b.log run
  local a
  a=$(wait_ack verbs-a $R publish 'b2-n1' 120) || { bad "the re-run never acked: $a"; return 1; }
  [ "$(ajq "$a" .status)" = "ok" ] || { bad "re-run ack is $(ajq "$a" .status), not ok"; return 1; }
  local aseq mnow
  aseq=$(printf '%s' "$a" | jq -r '.seq')
  mnow=$(mseq $P)
  [ "$aseq" = "$mnow" ] || { bad "the ack names seq $aseq, the installed manifest is $mnow"; return 1; }
  [ "$aseq" -gt "$base" ] || { bad "the ack names the PRE-CRASH baseline ($aseq <= $base) — persisted state, not the re-run"; return 1; }
  objexists "$P/files/g0400.txt" || { bad "the re-run did not finish the upload set"; return 1; }
  ok "re-run acked seq $aseq (baseline was $base) and the full set is cited"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B3  Rate limit + coalescing: a storm collapses into few barriers and
#     the final ack still covers EVERY touch, including one from the
#     middle of the storm.
# ─────────────────────────────────────────────────────────────────────
b3_storm_coalesces_and_covers() {
  local P=tenants/b03 R=/work/b03
  inpod verbs-a "mkdir -p $R" > /dev/null
  mkfiles verbs-a $R 3 h > /dev/null
  sy_bg verbs-a $P $R "FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS=5" /tmp/b03.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null

  # 100 touches in ~10 s, each with its own nonce, driven IN the pod so
  # the rate is real rather than kubectl-exec-bound. A file written
  # BEFORE the mid-storm touch is the at-least guarantee's subject.
  local t0 t1
  t0=$(date +%s)
  inpod verbs-a "mkdir -p $R/.flint; i=1; while [ \$i -le 100 ]; do \
      [ \$i -eq 50 ] && printf mid > $R/midstorm.txt; \
      printf '{\"nonce\":\"b3-%03d\"}' \$i > $R/.flint/publish.tmp; \
      mv $R/.flint/publish.tmp $R/.flint/publish; i=\$((i+1)); sleep 0.1; done; echo stormed" > /dev/null
  t1=$(date +%s)
  local span=$((t1 - t0))
  [ "$span" -le 30 ] || { bad "the storm took ${span}s — too slow to beat a 5 s min-interval"; return 1; }
  inpod verbs-a "test -f $R/midstorm.txt" || { bad "the mid-storm write never happened"; return 1; }
  ok "100 touches in ${span}s (min-interval 5 s), mid-storm file written at touch 50"

  local a
  a=$(wait_ack verbs-a $R publish 'b3-100' 60) || { bad "the final touch was never covered: $a"; return 1; }

  # Coalescing: at most ceil(span/5)+1 barriers may have run, and the
  # touch:barrier ratio must be nothing like 1:1.
  local cap=$(( (span + 4) / 5 + 1 )) touches=100
  local seq n
  seq=$(mseq $P)
  n=$(ajq "$a" '.nonces|length')
  [ "$seq" -le "$cap" ] || { bad "$seq barriers for a ${span}s storm, cap is $cap — coalescing did not hold"; return 1; }
  # Coalescing asserted on the RELIABLE signal: 100 touches must not
  # produce anything like 100 barriers. The previous form asserted
  # `nonces > 1`, which this leg's own note (below) says an agent
  # cannot rely on — the nonce list is best-effort, because a mid-storm
  # nonce appears in the ack of ITS OWN honor and that ack is then
  # overwritten. It duly went red on a slower run (12 s instead of 10 s,
  # under cluster contention) with the product behaving correctly: 100
  # touches, 1 nonce in the final ack, and the barrier count still
  # inside the cap. An assertion the documented contract does not
  # support is a flake by construction.
  [ "$((seq * 10))" -le "$touches" ] || {
    bad "$touches touches produced $seq barriers — that is not coalescing, whatever the cap allows"
    return 1; }
  ok "$seq barrier(s) for $touches touches, <= cap $cap — coalescing on the barrier count"
  [ "$n" -gt 1 ] && ok "the final ack also carried $n coalesced nonces" \
    || note "the final ack carried $n nonce; best-effort by design (see below), not asserted"

  # Coverage is the AT-LEAST guarantee, not the nonce list. The nonce
  # set is per-honor by construction (a retired pending record starts
  # the next window empty), so a mid-storm nonce appears in the ack of
  # ITS OWN honor and that ack is then overwritten. What an agent can
  # rely on is that a boundary later than its touch carries its work —
  # and `sentinel_mtime_unix_ns` is the covering signal, not `nonces`.
  # (The plan's B3 row asks for both a coalesced barrier count and one
  # ack naming all 100 touches; those cannot both hold. Corrected.)
  local cited
  cited=$(manif $P | jq -r '.entries|keys[]' | grep -c '^midstorm.txt$')
  [ "$cited" = "1" ] || { bad "the file written mid-storm is NOT cited at the final boundary"; return 1; }
  ok "the mid-storm write is cited at seq $seq — the at-least guarantee held across the storm"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B4  Stale-ack discrimination. Both halves must be observed: the stale
#     ack has to EXIST with an older nonce before the touch, or the leg
#     proves only that acks appear.
# ─────────────────────────────────────────────────────────────────────
b4_stale_ack_is_discriminated() {
  local P=tenants/b04 R=/work/b04
  inpod verbs-a "mkdir -p $R/.flint" > /dev/null
  mkfiles verbs-a $R 2 k > /dev/null
  # A plausible-looking ack from a previous life.
  inpod verbs-a "printf '%s' '{\"status\":\"ok\",\"nonces\":[\"b4-OLD\"],\"sentinel_mtime_unix_ns\":1,\"boundary\":\"sentinel\",\"completed_unix\":1,\"report\":{\"uploaded\":0,\"deleted\":0,\"parked\":0,\"consumed\":0,\"no_change\":true}}' > $R/.flint/publish.ack"
  local pre
  pre=$(ackf verbs-a $R publish)
  has 'b4-OLD' "$pre" || { bad "the stale ack was not planted"; return 1; }
  ok "stale ack planted with nonce b4-OLD"

  sy_bg verbs-a $P $R "" /tmp/b04.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  touchp verbs-a $R publish '{"nonce":"b4-NEW"}'
  local a
  a=$(wait_ack verbs-a $R publish 'b4-NEW' 40) || { bad "the fresh ack never landed: $a"; return 1; }
  has 'b4-OLD' "$a" && { bad "the fresh ack still carries the stale nonce"; return 1; }
  local ns
  ns=$(printf '%s' "$a" | jq -r '.sentinel_mtime_unix_ns')
  [ "$ns" -gt 1 ] || { bad "the ack's covering mtime is still the stale one ($ns)"; return 1; }
  ok "fresh ack replaced it: nonce b4-NEW, covering mtime $ns"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B5  Scoped sync (D4): an out-of-scope foreign change is DEFERRED to
#     the inbox flow, not lost. All three observations or the leg
#     proves nothing.
# ─────────────────────────────────────────────────────────────────────
b5_scoped_sync_defers_out_of_scope() {
  local P=tenants/b05 RA=/work/b05 RB=/work/b05
  sy verbs-a $P $RA "" checkout > /dev/null
  inpod verbs-a "mkdir -p $RA/inputs $RA/outputs" > /dev/null
  inpod verbs-a "printf A1 > $RA/inputs/a.txt; printf B1 > $RA/outputs/b.txt" > /dev/null
  sy verbs-a $P $RA "" barrier > /dev/null
  [ "$(objcat $P/files/inputs/a.txt)" = "A1" ] || { bad "the seed barrier did not publish inputs/a.txt"; return 1; }

  # A genuinely FOREIGN writer: a second pod with its own emptyDir, so
  # it takes the lease over instead of self-recognizing it.
  sy verbs-b $P $RB "" checkout > /dev/null
  # Lengths differ: a same-size, same-second rewrite is invisible to
  # the scan by design, and a fixture built on one tests the residual.
  inpod verbs-b "printf A2-remote > $RB/inputs/a.txt; printf B2-remote > $RB/outputs/b.txt" > /dev/null
  sy verbs-b $P $RB "" barrier > /dev/null
  [ "$(objcat $P/files/outputs/b.txt)" = "B2-remote" ] || { bad "the foreign writer did not install outputs/b.txt"; return 1; }
  ok "foreign install: inputs/a.txt=A2-remote and outputs/b.txt=B2-remote are in the bucket"

  # Scope = inputs/ only.
  sy_bg verbs-a $P $RA "" /tmp/b05.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $RA/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  touchp verbs-a $RA sync '{"nonce":"b5-s1","scope":["inputs/"]}'
  local a
  a=$(wait_ack verbs-a $RA sync 'b5-s1' 60) || { bad "no sync ack: $a"; return 1; }
  local la lb
  la=$(inpod verbs-a "cat $RA/inputs/a.txt")
  lb=$(inpod verbs-a "cat $RA/outputs/b.txt")
  [ "$la" = "A2-remote" ] || { bad "the IN-scope change was not applied (inputs/a.txt=$la)"; return 1; }
  [ "$lb" = "B1" ] || { bad "the OUT-of-scope change was applied anyway (outputs/b.txt=$lb) — scope means nothing"; return 1; }
  local oos
  oos=$(ajq "$a" '.report.out_of_scope_foreign')
  [ "$oos" -ge 1 ] || { bad "the ack reports $oos out-of-scope foreign changes — the deferral is invisible"; return 1; }
  ok "scoped: inputs/ applied (A2-remote), outputs/ untouched (B1), ack defers $oos change(s)"

  # The third observation: the deferred change integrates LATER.
  #
  # TWO barriers, and the reason is the D4 rule itself: the first
  # barrier's MERGE is what discovers the out-of-scope foreign entry and
  # queues it into the inbox; `consume_inbox` runs at the START of a
  # barrier, so it is the SECOND one that applies it to the tree. One
  # barrier would leave the change queued and the leg would read that as
  # loss.
  touchp verbs-a $RA publish '{"nonce":"b5-p1"}'
  wait_ack verbs-a $RA publish 'b5-p1' 60 > /dev/null || { bad "the first barrier never acked"; return 1; }
  sleep 6   # clear the 5 s min-interval so the second touch is its own barrier
  touchp verbs-a $RA publish '{"nonce":"b5-p2"}'
  wait_ack verbs-a $RA publish 'b5-p2' 60 > /dev/null || { bad "the second barrier never acked"; return 1; }
  local i
  for i in $(seq 1 30); do
    lb=$(inpod verbs-a "cat $RA/outputs/b.txt")
    [ "$lb" = "B2-remote" ] && break
    sleep 1
  done
  [ "$lb" = "B2-remote" ] || { bad "the deferred foreign change was LOST (outputs/b.txt=$lb)"; return 1; }
  ok "the deferred change integrated at the next barrier (outputs/b.txt=B2-remote)"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B6  Conflict transport: the report rides the ack in FULL, and the
#     local bytes are never the silent loser.
# ─────────────────────────────────────────────────────────────────────
b6_conflict_rides_the_ack() {
  local P=tenants/b06 R=/work/b06
  sy verbs-a $P $R "" checkout > /dev/null
  inpod verbs-a "mkdir -p $R" > /dev/null
  inpod verbs-a "printf V1 > $R/x.txt" > /dev/null
  sy verbs-a $P $R "" barrier > /dev/null
  sy verbs-b $P $R "" checkout > /dev/null
  inpod verbs-b "printf REMOTE > $R/x.txt" > /dev/null
  sy verbs-b $P $R "" barrier > /dev/null
  [ "$(objcat $P/files/x.txt)" = "REMOTE" ] || { bad "the foreign change never landed"; return 1; }

  # Local dirt, observable to the scan (size differs, so no mtime race).
  inpod verbs-a "printf LOCAL-DIRTY > $R/x.txt" > /dev/null
  local pre
  pre=$(inpod verbs-a "cat $R/x.txt")
  [ "$pre" = "LOCAL-DIRTY" ] || { bad "the fixture did not dirty the file"; return 1; }
  ok "local dirt (LOCAL-DIRTY) against a foreign remote (REMOTE)"

  sy_bg verbs-a $P $R "" /tmp/b06.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  touchp verbs-a $R sync '{"nonce":"b6-s1"}'
  local a
  a=$(wait_ack verbs-a $R sync 'b6-s1' 60) || { bad "no sync ack: $a"; return 1; }
  local n
  n=$(ajq "$a" '.report.conflicts|length')
  [ "$n" -ge 1 ] || { bad "the ack carries $n conflicts — the report did not survive the file transport"; return 1; }
  local post
  post=$(inpod verbs-a "cat $R/x.txt")
  [ "$post" = "LOCAL-DIRTY" ] || { bad "the sync overwrote the agent's dirty bytes (x.txt=$post)"; return 1; }
  ok "ack carries $n conflict record(s); the agent's bytes are untouched"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B7  remote.seq is LOCAL-ONLY news. The probe reads it from a
#     container with no credentials and no S3 client — a probe holding
#     creds could not prove the claim.
#
#     THE FOREIGN WRITER HAS TO BE THE GATEWAY, and that is a product
#     fact rather than a rig convenience: the lease admits exactly one
#     syncer, so while this workspace's syncer runs, the only party
#     that can put new bytes in the bucket is one that holds no lease
#     and edits no manifest — the HITL/gateway path. A second syncer
#     would block in `claim` forever (the first draft of this leg did,
#     and hung the drill rather than failing it).
# ─────────────────────────────────────────────────────────────────────
b7_ticker_is_local_only_news() {
  local P=tenants/b07 R=/work/b07
  inpod verbs-a "mkdir -p $R" > /dev/null
  inpod verbs-a "printf T1 > $R/t.txt" > /dev/null
  # A NORMAL floor here: the ticker rides the barrier's own HEAD and
  # never issues a request of its own, so an hour-long floor would
  # starve the mechanism under test.
  sy_bg verbs-a $P $R "FLINT_SYNC_FLOOR_SECS=5" /tmp/b07.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null

  probe_seq() { $K exec verbs-a -c probe -- cat $R/.flint/remote.seq 2>/dev/null; }
  local i t before u1 u2
  for i in $(seq 1 40); do
    t=$(probe_seq); [ -n "$t" ] && break; sleep 1
  done
  [ -n "$t" ] || { bad "the ticker file never appeared"; return 1; }
  before=$(printf '%s' "$t" | jq -r '.observed_seq')
  u1=$(printf '%s' "$t" | jq -r '.updated_unix')
  # The probe container carries no AWS_* env and no S3 client at all —
  # assert it rather than trusting the manifest, because this is the
  # entire claim the ticker makes.
  local creds
  creds=$($K exec verbs-a -c probe -- sh -c 'env | grep -c AWS_ || true' 2>/dev/null | tr -d '\r')
  [ "$creds" = "0" ] || { bad "the probe container carries $creds AWS_* variables — it could have read the bucket"; return 1; }
  ok "probe (no creds, no S3 client) reads the ticker: observed_seq=$before"

  # Heartbeat: updated_unix must advance across an IDLE tick — without
  # it an agent cannot tell "no news" from "syncer dead".
  sleep 12
  u2=$(probe_seq | jq -r '.updated_unix')
  [ "$u2" -gt "$u1" ] || { bad "updated_unix did not advance across an idle tick ($u1 -> $u2)"; return 1; }
  local mid
  mid=$(probe_seq | jq -r '.observed_seq')
  [ "$mid" = "$before" ] || { bad "observed_seq moved with no foreign write ($before -> $mid)"; return 1; }
  ok "idle heartbeat: updated_unix $u1 -> $u2, observed_seq still $before"

  # The foreign write, through the gateway, while the syncer holds the
  # lease and keeps ticking.
  local code
  code=$(gw_put_ws b07 hitl.txt "from-a-party-that-holds-no-lease")
  [ "$code" = "200" ] || { bad "the gateway HITL PUT returned $code: $(gw_body)"; return 1; }
  [ "$(objcat $P/files/hitl.txt)" = "from-a-party-that-holds-no-lease" ] || { bad "the HITL bytes never reached the bucket"; return 1; }

  local after
  for i in $(seq 1 60); do
    after=$(probe_seq | jq -r '.observed_seq')
    [ -n "$after" ] && [ "$after" -gt "$before" ] && break
    sleep 1
  done
  [ "$after" -gt "$before" ] || { bad "the foreign write never reached the ticker ($before -> $after)"; return 1; }
  # …and it reached the AGENT's tree too, which is what makes the news
  # actionable rather than merely true.
  inpod verbs-a "test -f $R/hitl.txt" || { bad "the ticker moved but the file never integrated"; return 1; }
  ok "foreign write seen by the probe alone: observed_seq $before -> $after"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B8  Zero-added-cost when unused. The control is the SAME workspace
#     with sentinels OFF: a magic number would drift, a delta cannot.
# ─────────────────────────────────────────────────────────────────────
b8_unused_verbs_cost_nothing() {
  local P=tenants/b08 R=/work/b08
  inpod verbs-a "mkdir -p $R" > /dev/null

  # <window secs> <extra env> <tag> -> requests touching this prefix.
  # The tag keeps each run's raw trace, because the SHAPE assertion
  # below reads the control run's after the second call has run.
  count_window() {
    local secs=$1 envs=$2 tag=$3
    mcx mc rm --recursive --force --versions "m/$BUCKET/$P/" > /dev/null 2>&1
    inpod verbs-a "rm -rf $R && mkdir -p $R" > /dev/null
    sy_bg verbs-a $P $R "FLINT_SYNC_FLOOR_SECS=5 $envs" /tmp/b08.log run
    inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
    # Trace only AFTER startup, so claim/checkout/marker writes are not
    # counted as steady-state cost.
    $K -n flint-system exec mc -- sh -c \
      "timeout $secs mc admin trace --json m > /tmp/b08-$tag.json 2>/dev/null; true" > /dev/null 2>&1
    killsync verbs-a
    await_exit verbs-a flint-sync 15 > /dev/null
    # Counted HERE, not in the mc pod: that image ships `mc` and little
    # else — no grep, no awk, no sed — so an in-pod count returns the
    # empty string and every budget assertion passes on nothing. The
    # leg's own anti-vacuity guard is what caught it.
    $K -n flint-system exec mc -- cat /tmp/b08-$tag.json 2>/dev/null \
      | grep -c "\"path\":\"/$BUCKET/$P" || true
  }

  # Requests of one (api, cell) kind in a saved trace. Counted OUT of
  # the pod for the same reason the totals are: minio/mc ships no grep.
  kind_count() { # <tag> <api> <cell>
    $K -n flint-system exec mc -- cat /tmp/b08-$1.json 2>/dev/null \
      | grep -c "\"api\":\"s3.$2\".*\"path\":\"/$BUCKET/$P/.flint/lean/$3\"" || true
  }

  local win=22 ticks=4
  local off_n auto_n
  off_n=$(count_window $win "FLINT_SYNC_SENTINELS=off" off)
  auto_n=$(count_window $win "" auto)
  [ "$off_n" -gt 0 ] || { bad "the control window counted ZERO requests — a dead oracle passes every budget"; return 1; }
  ok "control (sentinels off): $off_n requests in ${win}s ≈ $((off_n / ticks))/tick"

  # U7: this leg computed a per-tick figure, printed it, and asserted
  # NOTHING about it. A delta-only oracle passes a syncer that
  # regressed from 4 requests per tick to 40, so long as sentinels
  # added none of them — and §7 says the draft's own "1 HEAD" figure
  # was ~19x under, so the absolute number is exactly the thing that
  # has been wrong before.
  #
  # The plan states the idle tick EXACTLY (§7, and B8's own acceptance
  # row): 4 requests at floor <= 30 s — the renew CAS and the deposal
  # read, both on .flint/lean/epoch with the renew PUT-priced, plus the
  # inbox GET and the manifest HEAD. That shape is asserted here, which
  # is what "a recorded control whose request shape is written into the
  # leg" was asking for.
  local per_tick=$(( off_n / ticks ))
  [ "$per_tick" -ge 4 ] && [ "$per_tick" -le 6 ] || {
    bad "the idle tick costs $per_tick requests, outside the measured 5 (window $win s, $ticks ticks, $off_n total)"
    return 1; }
  ok "idle tick costs $per_tick requests"

  local n_renew n_epoch n_inbox n_manifest
  n_renew=$(kind_count off PutObject epoch)
  n_epoch=$(kind_count off GetObject epoch)
  n_inbox=$(kind_count off GetObject inbox)
  n_manifest=$(kind_count off HeadObject manifest)
  # Every category must appear, or "4 requests" could be four of the
  # wrong thing — four manifest GETs on a 264 MiB manifest is the same
  # count and a different product.
  local missing=""
  [ "$n_renew" -gt 0 ]    || missing="$missing renew-PUT"
  [ "$n_epoch" -gt 0 ]    || missing="$missing epoch-GET"
  [ "$n_inbox" -gt 0 ]    || missing="$missing inbox-GET"
  [ "$n_manifest" -gt 0 ] || missing="$missing manifest-HEAD"
  [ -z "$missing" ] || {
    bad "the idle tick's shape is not the documented one — missing:$missing (renew=$n_renew epoch=$n_epoch inbox=$n_inbox manifest=$n_manifest)"
    return 1; }
  # And the manifest is read by HEAD, never by GET: the 0b lever that
  # took the 1M-file idle tick from 27.5 s to 1.85 s was exactly this,
  # and a regression to GET is invisible in a request COUNT.
  local n_mget
  n_mget=$(kind_count off GetObject manifest)
  [ "$n_mget" -eq 0 ] || {
    bad "$n_mget idle-tick manifest GETs — the HEAD+etag lever regressed; at 1M files this is 264 MiB per tick"
    return 1; }
  ok "shape: renew PUT=$n_renew, epoch GET=$n_epoch, inbox GET=$n_inbox, manifest HEAD=$n_manifest, manifest GET=0"

  # The renew arm fires TWICE at this floor, and that is correct.
  # §7's "4 requests" is the tick's SHAPE (renew + epoch read + inbox
  # GET + manifest HEAD); the renew COUNT is floor-dependent, because
  # D12's heartbeat interval is min(floor,30) and the floor arm renews
  # on its own independent non-resettable timer with no debounce in
  # `lease::renew`. At floor=60 that is 3 renews/minute, which §7's
  # delta line prices at +100 PUT/s fleet-wide. At THIS leg's floor=5
  # the two arms coincide, so it is 2 per tick and the tick costs 5.
  # Asserting a bare total would therefore encode a floor-specific
  # number as if it were the contract; asserting the multiplier catches
  # the regression that actually matters — the second renew silently
  # disappearing, which would be a takeover-safety loss (§2.1a), not a
  # saving.
  [ "$n_renew" -ge $(( n_epoch * 2 )) ] || {
    bad "renew PUT=$n_renew against epoch GET=$n_epoch — the heartbeat arm's independent renew is gone; D12 bought takeover safety with exactly that PUT"
    return 1; }
  ok "the renew arm fires twice per tick ($n_renew PUTs vs $n_epoch epoch reads) — D12's heartbeat plus the floor arm, priced in §7"

  local delta=$(( auto_n - off_n ))
  [ "$delta" -lt 0 ] && delta=$(( -delta ))
  [ "$delta" -le "$ticks" ] || { bad "sentinels-on cost $auto_n vs $off_n — an idle workspace pays ${delta} extra requests"; return 1; }
  ok "sentinels on: $auto_n requests — delta $delta over $ticks ticks"

  # …and the reserved namespace never becomes bucket state (D0).
  local leaked
  leaked=$(allkeys "$P" | grep -c "files/.flint/" || true)
  [ "$leaked" = "0" ] || { bad "$leaked object(s) under files/.flint/ — the control namespace was published"; return 1; }
  inpod verbs-a "test -f $R/.flint/publish.ack" && { bad "an ack appeared with no sentinel ever touched"; return 1; }
  ok "no files/.flint/ objects, no acks: the verbs are inert when unused"
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B16  Hybrid default-floor regression: the rewritten select/interval
#      loop must not have starved cadence. No sentinel anywhere.
# ─────────────────────────────────────────────────────────────────────
b16_default_floor_still_publishes() {
  local P=tenants/b16 R=/work/b16
  inpod verbs-a "mkdir -p $R" > /dev/null
  sy_bg verbs-a $P $R "FLINT_SYNC_FLOOR_SECS=10" /tmp/b16.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local s0
  s0=$(mseq $P)
  mkfiles verbs-a $R 3 c > /dev/null

  # floor + slack. The slack is generous on purpose: the claim is "the
  # loop still ticks", not "the loop ticks to the second".
  wait_seq_gt $P "$s0" 40 || { bad "the manifest never advanced on the default floor (still seq $(mseq $P))"; return 1; }
  local s1
  s1=$(mseq $P)
  # It must be CADENCE, not a sentinel that leaked in from somewhere.
  local src
  src=$(bsource $P)
  [ "$src" = "cadence" ] || { bad "the boundary source is '$src', not cadence — the leg tested the wrong arm"; return 1; }
  inpod verbs-a "test -e $R/.flint/publish" && { bad "a publish sentinel existed during the leg"; return 1; }
  inpod verbs-a "test -e $R/.flint/publish.ack" && { bad "an ack exists — a sentinel was honored during a cadence-only leg"; return 1; }
  local cited
  cited=$(manif $P | jq -r '.entries|keys|length')
  [ "$cited" -ge 3 ] || { bad "cadence published a boundary citing only $cited files"; return 1; }
  ok "cadence advanced seq $s0 -> $s1 citing $cited files, source=cadence, no sentinel ever existed"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B22  A workspace that already had app-owned `.flint/` data. The
#      syncer must disable the verbs rather than eat the file.
# ─────────────────────────────────────────────────────────────────────
b22_preexisting_flint_disables_the_verbs() {
  local P=tenants/b22 R=/work/b22
  inpod verbs-a "rm -rf $R && mkdir -p $R/.flint" > /dev/null
  # An app's OWN file that happens to be named like a sentinel, with
  # bytes recorded so "unchanged" is checkable rather than assumed.
  local want='this is the application own publish config, not a sentinel'
  inpod verbs-a "printf '%s' '$want' > $R/.flint/publish" > /dev/null
  inpod verbs-a "printf app-data > $R/.flint/settings.json" > /dev/null
  local pre
  pre=$(inpod verbs-a "cat $R/.flint/publish")
  [ "$pre" = "$want" ] || { bad "the fixture did not plant the app's file"; return 1; }
  inpod verbs-a "test -e $R/.flint/capabilities.json" && { bad "the tree was already marked — this is not a pre-existing workspace"; return 1; }
  ok "pre-existing app-owned .flint/publish planted (${#want} bytes), tree never marked"

  sy_bg verbs-a $P $R "FLINT_SYNC_FLOOR_SECS=5" /tmp/b22.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local i c
  for i in $(seq 1 40); do
    c=$(caps verbs-a $R); [ -n "$c" ] && break; sleep 1
  done
  [ -n "$c" ] || { bad "the syncer never wrote a capability marker"; return 1; }
  local verbs reason
  verbs=$(printf '%s' "$c" | jq -r '.verbs|length')
  reason=$(printf '%s' "$c" | jq -r '.reason // empty')
  [ "$verbs" = "0" ] || { bad "the marker advertises $verbs verb(s) over a pre-existing .flint/ tree"; return 1; }
  [ "$reason" = "preexisting-flint-paths" ] || { bad "the marker's reason is '$reason', not preexisting-flint-paths"; return 1; }
  ok "capabilities.json: verbs=[] reason=$reason"

  # Not consumed, not published, byte-identical.
  sleep 12
  local post
  post=$(inpod verbs-a "cat $R/.flint/publish 2>/dev/null")
  [ "$post" = "$want" ] || { bad "the app's file was consumed or rewritten (now: '$post')"; return 1; }
  inpod verbs-a "test -e $R/.flint/publish.ack" && { bad "the syncer acked a file it was never given"; return 1; }
  local leaked
  leaked=$(allkeys "$P" | grep -c "files/.flint/" || true)
  [ "$leaked" = "0" ] || { bad "$leaked object(s) under files/.flint/ — the app's control data was published"; return 1; }
  ok "the app's file is untouched, unacked and unpublished after a full floor"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B11a  preStop settles what it owes: SIGTERM with a held sentinel ⇒
#       the final boundary publishes the declared bytes and acks it.
# ─────────────────────────────────────────────────────────────────────
b11a_sigterm_settles_an_owed_ack() {
  # An hour-long floor and a long min-interval: the
  # sentinel arm consumes the touch and then must HOLD it, so the
  # pending record still stands when SIGTERM arrives. That is the
  # container-restart case D10 names — the emptyDir and the agent both
  # survive, and a waiting agent must not be stranded.
  local P2=tenants/b11a2 R2=/work/b11a2
  inpod verbs-a "rm -rf $R2 && mkdir -p $R2" > /dev/null
  sy_bg verbs-a $P2 $R2 "FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS=600" /tmp/b11a2.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R2/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  inpod verbs-a "printf one > $R2/one.txt" > /dev/null
  touchp verbs-a $R2 publish '{"nonce":"b11a-first"}'
  wait_ack verbs-a $R2 publish 'b11a-first' 60 > /dev/null || { bad "the first sentinel never acked"; return 1; }

  # The second touch is now inside the 600 s min-interval, and the floor
  # is an hour away: it can only be settled by the drain.
  inpod verbs-a "printf two > $R2/two.txt" > /dev/null
  touchp verbs-a $R2 publish '{"nonce":"b11a-owed"}'
  local pend
  for i in $(seq 1 25); do
    pend=$(inpod verbs-a "ls $R2/.flint-sync/publish.pending.json 2>/dev/null")
    [ -n "$pend" ] && break
    sleep 1
  done
  [ -n "$pend" ] || { bad "the second sentinel was never consumed — the drain would have nothing owed"; return 1; }
  has 'b11a-owed' "$(ackf verbs-a $R2 publish)" && { bad "the held sentinel was honored anyway — the min-interval did not hold"; return 1; }
  local s2
  s2=$(mseq $P2)
  ok "a consumed-but-unhonored sentinel stands at seq $s2 (held by a 600 s min-interval, floor an hour away)"

  termsync verbs-a
  await_exit verbs-a flint-sync 40 || { bad "the syncer never exited on SIGTERM"; return 1; }
  local a
  a=$(ackf verbs-a $R2 publish)
  has 'b11a-owed' "$a" || { bad "the owed ack was never settled: $a"; return 1; }
  [ "$(ajq "$a" .boundary)" = "drain" ] || { bad "the settled ack reads boundary=$(ajq "$a" .boundary), not drain"; return 1; }
  local s3 src2
  s3=$(mseq $P2); src2=$(bsource $P2)
  [ "$s3" -gt "$s2" ] || { bad "the drain settled the ack but installed no boundary"; return 1; }
  # The ack is a LOCAL file; the manifest is what the fleet reads. One
  # boundary must never name two clocks, least of all with the bucket
  # holding the wrong one.
  [ "$src2" = "drain" ] || { bad "the ack says drain and the bucket says '$src2'"; return 1; }
  [ "$(objcat $P2/files/two.txt)" = "two" ] || { bad "the drain acked but did not publish the declared bytes"; return 1; }
  ok "owed ack settled: seq $s2 -> $s3, ack and bucket BOTH read drain, declared bytes published"
  mcx mc rm --recursive --force --versions "m/$BUCKET/$P2/" > /dev/null 2>&1
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B13  Legacy `.flint/` upgrade safety. A pre-D0 syncer could cite a
#      path under the reserved namespace; an upgraded one must neither
#      eat it nor DELETE it for having fallen out of its scan.
# ─────────────────────────────────────────────────────────────────────
b13_legacy_citation_survives_the_upgrade() {
  local P=tenants/b13 R=/work/b13
  inpod verbs-a "rm -rf $R && mkdir -p $R" > /dev/null
  sy verbs-a $P $R "" checkout > /dev/null
  inpod verbs-a "printf normal > $R/normal.txt" > /dev/null
  sy verbs-a $P $R "" barrier > /dev/null
  local s0
  s0=$(mseq $P)
  [ "$s0" -ge 1 ] || { bad "the seed barrier did not install"; return 1; }

  # The pre-D0 state, planted in BOTH places it lives: the object in the
  # bucket, and the citation in the tree's baseline — which is exactly
  # what the pre-flight reads (`legacy_cited`).
  local want='legacy control data cited by a pre-D0 syncer'
  putobj "$P/files/.flint/legacy.txt" "$want"
  [ "$(objcat $P/files/.flint/legacy.txt)" = "$want" ] || { bad "the legacy object was not planted"; return 1; }
  # The syncer image is busybox: no jq. sed with a printf'd insert
  # line does the same job and keeps the fixture inside the image the
  # product actually ships.
  inpod verbs-a "printf '    \".flint/legacy.txt\": {\"etag\": \"legacy\", \"generation\": 1, \"size\": ${#want}, \"mtime_unix\": 1},\n' > /tmp/b13.ins" > /dev/null
  inpod verbs-a "sed '/\"entries\": {/r /tmp/b13.ins' $R/.flint-sync/baseline.json > /tmp/b13.bl && mv /tmp/b13.bl $R/.flint-sync/baseline.json" > /dev/null
  local planted
  planted=$(inpod verbs-a "grep -c '\.flint/legacy.txt' $R/.flint-sync/baseline.json" | tr -d ' \r')
  [ "$planted" = "1" ] || { bad "the legacy baseline citation could not be planted (matches=$planted)"; return 1; }
  ok "pre-upgrade state: files/.flint/legacy.txt in the bucket AND cited by the baseline"

  sy_bg verbs-a $P $R "FLINT_SYNC_FLOOR_SECS=5" /tmp/b13.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local i c
  for i in $(seq 1 40); do c=$(caps verbs-a $R); [ -n "$c" ] && break; sleep 1; done
  local verbs reason
  verbs=$(printf '%s' "$c" | jq -r '.verbs|length')
  reason=$(printf '%s' "$c" | jq -r '.reason // empty')
  [ "$verbs" = "0" ] || { bad "the upgraded syncer advertises $verbs verb(s) over a legacy citation"; return 1; }
  [ "$reason" = "preexisting-flint-paths" ] || { bad "the warning record reads '$reason'"; return 1; }
  ok "the upgrade disabled the verbs and said why: reason=$reason"

  # Two barriers, and the object must still be there afterwards: a
  # scan that no longer SEES a cited path must not conclude it was
  # deleted.
  inpod verbs-a "printf more > $R/more.txt" > /dev/null
  wait_seq_gt $P "$s0" 40 || { bad "the upgraded syncer never published again"; return 1; }
  local s1
  s1=$(mseq $P)
  wait_seq_gt $P "$s1" 40 > /dev/null
  local post
  post=$(objcat "$P/files/.flint/legacy.txt")
  [ "$post" = "$want" ] || { bad "the legacy object was destroyed or rewritten (now: '$post')"; return 1; }
  ok "after two barriers the legacy object survives byte-identical"
  killsync verbs-a
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B17  Renewal under storm. A sentinel storm must never starve the
#      heartbeat into letting a standby depose a live syncer.
# ─────────────────────────────────────────────────────────────────────
b17_renewal_survives_a_sentinel_storm() {
  local P=tenants/b17 R=/work/b17 RB=/work/b17b
  inpod verbs-a "mkdir -p $R" > /dev/null
  sy_bg verbs-a $P $R "" /tmp/b17.log run          # floor 3600: cadence is out
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local e0
  e0=$(epochdoc $P | jq -r '.epoch')
  [ -n "$e0" ] || { bad "no epoch cell in the bucket"; return 1; }

  # The standby: a real claimant, blocked in `claim`, counting quiet
  # polls. A DEAD standby would prove nothing — the leg reads its log.
  inpod verbs-b "rm -rf $RB && mkdir -p $RB" > /dev/null
  sy_bg verbs-b $P $RB "" /tmp/b17b.log checkout

  # The storm: a touch every 0.2 s for ~90 s.
  inpod verbs-a "nohup sh -c 'mkdir -p $R/.flint; i=1; while [ \$i -le 450 ]; do \
      printf \"{\\\"nonce\\\":\\\"b17-%d\\\"}\" \$i > $R/.flint/publish.tmp; \
      mv $R/.flint/publish.tmp $R/.flint/publish; \
      printf storm-\$i > $R/storm.txt; i=\$((i+1)); sleep 0.2; done' > /dev/null 2>&1 & echo stormed" > /dev/null

  # Renewals are counted as CHANGES to the epoch object's Last-Modified,
  # and the gaps are measured on the drill's own wall clock — which is
  # what "renewals at <=30 s cadence" means to whoever is watching a
  # fleet.
  # `renew_every = min(floor_secs, 30)`, and this leg runs the hour-long
  # floor, so renewals land every 30 s. Sampling at 3 s means an
  # observed gap of 30 s reads as 30-33; the bound that MATTERS is the
  # takeover threshold — six quiet polls at 10 s — so a gap comfortably
  # under 60 s is what keeps a live syncer from being deposed.
  local i renews="" last_seen="" last_at=0 gap_max=0 now
  for i in $(seq 1 45); do
    local lm
    lm=$(epoch_mtime $P)
    now=$(date +%s)
    if [ -n "$lm" ] && [ "$lm" != "$last_seen" ]; then
      if [ "$last_at" != "0" ]; then
        local g=$((now - last_at))
        [ "$g" -gt "$gap_max" ] && gap_max=$g
      fi
      last_seen=$lm
      last_at=$now
      renews="$renews."
    fi
    sleep 3
  done
  local n_renews=${#renews}
  local e1
  e1=$(epochdoc $P | jq -r '.epoch')
  [ "$e1" = "$e0" ] || { bad "the epoch moved $e0 -> $e1 — the standby DEPOSED a live syncer under storm"; return 1; }

  # The standby was genuinely watching.
  local sb
  sb=$(inpod verbs-b "cat /tmp/b17b.log 2>/dev/null | grep -c 'waiting on the standing lease'" | tr -d ' ')
  [ -n "$sb" ] && [ "$sb" -ge 3 ] || { bad "the standby logged $sb quiet polls — it was not really claiming"; return 1; }
  ok "no takeover across ~90 s of storm: epoch stayed $e0, standby observed $sb quiet polls"

  [ "$n_renews" -ge 3 ] || { bad "only $n_renews renewals observed across ~135 s — the heartbeat starved under storm"; return 1; }
  [ "$gap_max" -gt 0 ] || { bad "no renewal gap could be measured — the oracle saw nothing move"; return 1; }
  [ "$gap_max" -lt 60 ] || { bad "the widest renewal gap was ${gap_max}s, past the 60 s quiet-poll takeover window — a standby could have deposed a LIVE syncer"; return 1; }
  ok "$n_renews renewals, widest gap ${gap_max}s (30 s cadence, 60 s takeover window) while the storm ran"
  inpod verbs-a "pkill -f 'while \[ ' 2>/dev/null; true" > /dev/null
  killsync verbs-a; killsync verbs-b
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B18  Deposed mid-pending ⇒ a REFUSED ack. A zombie must never leave
#      an agent waiting on a marker that will never be answered.
# ─────────────────────────────────────────────────────────────────────
b18_deposed_mid_pending_refuses() {
  local P=tenants/b18 RS=/work/b18 RB=/work/b18s
  inpod verbs-s "rm -rf $RS" > /dev/null
  inpod verbs-s2 "rm -rf $RB" > /dev/null
  sy_bg verbs-s $P $RS "" /tmp/b18.log run
  inpod verbs-s "for i in \$(seq 1 60); do [ -f $RS/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  mkfiles verbs-s $RS 250 z > /dev/null

  # A pending sentinel, then freeze it MID-HONOR so the record stands.
  #
  # Polling for the pending record and then signalling is a race the
  # honor wins: the record is there when the poll looks, and the ack is
  # written before SIGSTOP lands. Freeze inside the UPLOAD LOOP instead,
  # which the two-HEAD check pins down exactly — 250 zero-padded files
  # walked at fan-out 1, so an early key present and the last key absent
  # means the honor is in flight and no ack can exist yet.
  touchp verbs-s $RS publish '{"nonce":"b18-pending"}'
  wait_key $P "z0002.txt" 300 || { bad "the honor never started uploading — nothing to strand"; return 1; }
  objexists "$P/files/z0250.txt" && { bad "the honor finished before the freeze — nothing was mid-flight"; return 1; }
  stopsync verbs-s
  local pend
  pend=$(inpod verbs-s "ls $RS/.flint-sync/publish.pending.json 2>/dev/null")
  [ -n "$pend" ] || { bad "no pending record at freeze time — the sentinel was never consumed"; return 1; }
  inpod verbs-s "test -f $RS/.flint/publish.ack" && { bad "an ack already existed at freeze time"; return 1; }
  local hash_before
  hash_before=$(treehash verbs-s $RS)
  ok "pending record standing at deposal, no ack, tree hash recorded"

  # The successor deposes it for real.
  sy_bg verbs-s2 $P $RB "" /tmp/b18s.log run
  await_file verbs-s2 "$RB/.flint-sync/checkout-complete" "$TAKEOVER_SECS" \
    || { bad "the successor never took over"; contsync verbs-s; return 1; }
  inpod verbs-s2 "printf successor > $RB/succ.txt" > /dev/null
  touchp verbs-s2 $RB publish '{"nonce":"b18-succ"}'
  wait_ack verbs-s2 $RB publish 'b18-succ' 90 > /dev/null || { bad "the successor never published"; contsync verbs-s; return 1; }
  local newepoch
  newepoch=$(epochdoc $P | jq -r '.epoch')
  ok "successor holds epoch $newepoch and published"

  # Thaw the zombie: it must settle the owed ack as a REFUSAL.
  contsync verbs-s
  local a
  a=$(wait_ack verbs-s $RS publish 'refused-fenced' 90) || { bad "the zombie never settled the owed ack: $a"; return 1; }
  local st ep
  st=$(ajq "$a" .status); ep=$(ajq "$a" '.observed_epoch // empty')
  [ "$st" = "refused-fenced" ] || { bad "the zombie's ack status is '$st'"; return 1; }
  [ -n "$ep" ] || { bad "the refusal names no epoch — an agent cannot tell WHICH successor deposed its syncer"; return 1; }
  [ "$ep" = "$newepoch" ] || { bad "the refusal names epoch $ep, the successor holds $newepoch"; return 1; }
  local c
  c=$(caps verbs-s $RS)
  [ "$(printf '%s' "$c" | jq -r '.state')" = "fenced" ] || { bad "capabilities still reads '$(printf '%s' "$c" | jq -r .state)' on a deposed syncer"; return 1; }
  [ "$(printf '%s' "$c" | jq -r '.verbs|length')" = "0" ] || { bad "a fenced syncer still advertises verbs"; return 1; }
  ok "owed ack settled refused-fenced (epoch ${ep:-unnamed}); capabilities: state=fenced, verbs=[]"

  # A FURTHER sentinel on the zombie is refused too, and the tree is
  # not mutated by the zombie's death throes.
  inpod verbs-s "rm -f $RS/.flint/sync.ack" > /dev/null
  touchp verbs-s $RS sync '{"nonce":"b18-zombie-sync"}'
  sleep 15
  local sa hash_after
  sa=$(ackf verbs-s $RS sync)
  hash_after=$(treehash verbs-s $RS)
  if [ -n "$sa" ]; then
    [ "$(ajq "$sa" .status)" = "refused-fenced" ] || { bad "the zombie honored a sync sentinel: $(ajq "$sa" .status)"; return 1; }
  fi
  [ "$hash_after" = "$hash_before" ] || { bad "the zombie mutated its tree after being fenced"; return 1; }
  ok "a later sync sentinel on the zombie is refused${sa:+ (acked)}; tree hash unchanged"
  killsync verbs-s; killsync verbs-s2
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B25  Hot loops, no regression (D3.1). The meter must discriminate by
#      WORK, not by calls: the same touch rate over a big file throttles
#      and over a small one does not.
# ─────────────────────────────────────────────────────────────────────
b25_hot_loops_meter_by_work_not_by_calls() {
  local P=tenants/b25 R=/work/b25
  # `whole_put_max` is 64 MiB, so a 65 MiB rewrite is 2 units and a
  # budget of 4 exhausts in two honors. The budget is per ROLLING HOUR,
  # so (a) and (b) must not share a prefix or a state dir.
  # A deferred boundary is honored by the next FLOOR tick and its ack is
  # stamped there, so the hour-long floor this drill uses everywhere
  # else would strand exactly the ack this leg reads. 20 s instead.
  #
  # Budget 4 is what makes the two arms comparable: 65 MiB is
  # ceil(65/64) = 2 units per honor, so (a) exhausts after two honors and
  # defers the rest; 4 KiB is 1 unit, so (b) fits four honors inside the
  # same budget at the same touch rate. Same knob, same rate, opposite
  # outcome — which is the whole claim.
  local BUDGET="FLINT_SYNC_SENTINEL_HOURLY_BUDGET=4 FLINT_SYNC_SENTINEL_MIN_INTERVAL_SECS=1 FLINT_SYNC_FLOOR_SECS=20"

  # ── (a) the big-file loop ──
  inpod verbs-a "rm -rf $R && mkdir -p $R" > /dev/null
  sy_bg verbs-a $P $R "$BUDGET" /tmp/b25a.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local i deferred=0 honored=0
  for i in $(seq 1 4); do
    # 65 MiB, contents varying per iteration so it is genuinely dirty.
    inpod verbs-a "dd if=/dev/urandom of=$R/big.bin bs=1M count=65 2>/dev/null; printf 'i%d' $i >> $R/big.bin" > /dev/null
    touchp verbs-a $R publish "{\"nonce\":\"b25a-$i\"}"
    local a
    a=$(wait_ack verbs-a $R publish "b25a-$i" 240) || { bad "iteration $i was never acked: $a"; return 1; }
    local b
    b=$(ajq "$a" .boundary)
    [ "$b" = "sentinel-deferred" ] && deferred=$((deferred + 1))
    [ "$b" = "sentinel" ] && honored=$((honored + 1))
    sleep 2
  done
  killsync verbs-a
  await_exit verbs-a flint-sync 20 > /dev/null
  [ "$deferred" -ge 1 ] || { bad "4 x 65 MiB rewrites against a budget of 4 units produced NO sentinel-deferred ack (honored=$honored)"; return 1; }
  ok "(a) big-file loop: $honored honored, $deferred deferred — the meter engaged"

  # ── (b) the small-file loop, SAME touch rate ──
  local P2=tenants/b25b R2=/work/b25b
  mcx mc rm --recursive --force --versions "m/$BUCKET/$P2/" > /dev/null 2>&1
  inpod verbs-a "rm -rf $R2 && mkdir -p $R2" > /dev/null
  sy_bg verbs-a $P2 $R2 "$BUDGET" /tmp/b25b.log run
  inpod verbs-a "for i in \$(seq 1 60); do [ -f $R2/.flint-sync/checkout-complete ] && break; sleep 1; done" > /dev/null
  local sdef=0 shon=0
  for i in $(seq 1 4); do
    inpod verbs-a "dd if=/dev/urandom of=$R2/small.bin bs=1k count=4 2>/dev/null; printf 'i%d' $i >> $R2/small.bin" > /dev/null
    touchp verbs-a $R2 publish "{\"nonce\":\"b25b-$i\"}"
    local a
    a=$(wait_ack verbs-a $R2 publish "b25b-$i" 120) || { bad "small iteration $i was never acked: $a"; return 1; }
    local b
    b=$(ajq "$a" .boundary)
    [ "$b" = "sentinel-deferred" ] && sdef=$((sdef + 1))
    [ "$b" = "sentinel" ] && shon=$((shon + 1))
    sleep 2
  done
  killsync verbs-a
  [ "$sdef" = "0" ] || { bad "the 4 KiB loop was throttled $sdef time(s) at the SAME touch rate — the meter counts calls, not work"; return 1; }
  [ "$shon" -ge 4 ] || { bad "the 4 KiB loop honored only $shon of 4 touches at the same rate the big loop was throttled at"; return 1; }
  ok "(b) 4 KiB loop at the same touch rate: $shon honored, 0 deferred"
  ok "the meter discriminates by WORK: 65 MiB throttles at $deferred, 4 KiB at 0"
  # 4 x 65 MiB of versions is real disk on the node; give it back.
  mcx mc rm --recursive --force --versions "m/$BUCKET/$P2/" > /dev/null 2>&1
  mcx mc rm --recursive --force --versions "m/$BUCKET/$P/" > /dev/null 2>&1
  return 0
}

# ─────────────────────────────────────────────────────────────────────
# B14  Mixed-fleet detection, both directions — against a REAL pre-D0
#      binary built from `69b35978`, the commit before the boundary
#      verbs existed. It has no sentinel support, no capability marker,
#      and no `.flint/` reservation in its scan. The last of those is
#      the hazard, and it cannot be simulated by a knob: `SENTINELS=off`
#      on a CURRENT binary still reserves the namespace.
# ─────────────────────────────────────────────────────────────────────
b14_mixed_fleet_is_detectable_both_ways() {
  local PA=tenants/b14a PB=tenants/b14b PC=tenants/b14c
  local RA=/work/b14a RB=/work/b14b RC=/work/b14c
  local restore=$CONT

  $K get pod verbs-old > /dev/null 2>&1 || {
    bad "the verbs-old pod is not in the rig — B14 needs a real pre-D0 image (flint-sync:predo)"
    return 1
  }

  # ── (a) an old syncer leaves no marker, and PUBLISHES the control
  #        namespace. Both halves on isolated prefixes.
  CONT=sync
  inpod verbs-old "rm -rf $RA && mkdir -p $RA/.flint" > /dev/null
  local want='an agent sentinel the old binary has never heard of'
  inpod verbs-old "printf '%s' '$want' > $RA/.flint/publish; printf work > $RA/real.txt" > /dev/null
  sy verbs-old $PA $RA "" checkout > /dev/null
  sy verbs-old $PA $RA "" barrier > /dev/null
  [ "$(mseq $PA)" -ge 1 ] || { bad "the old binary never published"; CONT=$restore; return 1; }

  # The marker an agent library keys on is ABSENT — which is the whole
  # point: a library that requires it refuses to touch sentinels here.
  inpod verbs-old "test -e $RA/.flint/capabilities.json" && { bad "the old binary wrote a capability marker it cannot have"; CONT=$restore; return 1; }
  # …and the sentinel file was NOT consumed, because it means nothing to
  # this binary.
  [ "$(inpod verbs-old "cat $RA/.flint/publish")" = "$want" ] || { bad "the old binary consumed the sentinel"; CONT=$restore; return 1; }
  ok "old image: no capability marker, sentinel file untouched"

  # THE DESTRUCTIVE CONTROL, on its own prefix: the old binary has no
  # reserved namespace, so it publishes the agent's control file as
  # ordinary workspace data.
  local leaked_old
  leaked_old=$(allkeys "$PA" | grep -c "files/.flint/" || true)
  [ "$leaked_old" -ge 1 ] || { bad "the pre-D0 binary did NOT publish files/.flint/ — this image is not pre-D0 and the leg proves nothing"; CONT=$restore; return 1; }
  ok "destructive control: the old binary published $leaked_old key(s) under files/.flint/ — the hazard D0 exists to close"

  # The SAME fixture on the shipping binary, on its own prefix.
  CONT=new
  inpod verbs-old "rm -rf $RB && mkdir -p $RB/.flint" > /dev/null
  inpod verbs-old "printf '%s' '$want' > $RB/.flint/publish; printf work > $RB/real.txt" > /dev/null
  sy verbs-old $PB $RB "" checkout > /dev/null
  sy verbs-old $PB $RB "" barrier > /dev/null
  local leaked_new
  leaked_new=$(allkeys "$PB" | grep -c "files/.flint/" || true)
  [ "$leaked_new" = "0" ] || { bad "the shipping binary published $leaked_new key(s) under files/.flint/"; CONT=$restore; return 1; }
  [ "$(objcat $PB/files/real.txt)" = "work" ] || { bad "the shipping binary published nothing at all — the comparison is empty"; CONT=$restore; return 1; }
  ok "shipping image on the same fixture: 0 keys under files/.flint/, ordinary data published"

  # ── (b) downgrade over a LIVE tree: the marker survives the rollback,
  #        so the only tell an agent has is that its boot stamp did not
  #        move across an observed restart.
  CONT=new
  inpod verbs-old "rm -rf $RC && mkdir -p $RC" > /dev/null
  inpod verbs-old "printf live > $RC/live.txt" > /dev/null
  sy_bg verbs-old $PC $RC "FLINT_SYNC_FLOOR_SECS=5" /tmp/b14c.log run
  local i c
  for i in $(seq 1 40); do c=$(caps verbs-old $RC); [ -n "$c" ] && break; sleep 1; done
  [ -n "$c" ] || { bad "the shipping binary never wrote a marker"; CONT=$restore; return 1; }
  local boot1 state1
  boot1=$(printf '%s' "$c" | jq -c '.boot')
  state1=$(printf '%s' "$c" | jq -r '.state')
  [ -n "$boot1" ] && [ "$boot1" != "null" ] || { bad "the marker carries no boot stamp — the rollback tell does not exist"; CONT=$restore; return 1; }
  killsync verbs-old
  await_exit verbs-old flint-sync 20 > /dev/null
  ok "marked by the shipping binary: state=$state1, boot stamp recorded"

  # The rollback: the OLD binary now runs over the same tree.
  CONT=sync
  sy verbs-old $PC $RC "" checkout > /dev/null
  sy verbs-old $PC $RC "" barrier > /dev/null
  CONT=new
  local c2 boot2
  c2=$(caps verbs-old $RC)
  boot2=$(printf '%s' "$c2" | jq -c '.boot')
  [ -n "$c2" ] || { bad "the rollback DELETED the marker — an agent would see no syncer rather than a stale one"; CONT=$restore; return 1; }
  [ "$boot2" = "$boot1" ] || { bad "the boot stamp moved across a downgrade ($boot1 -> $boot2) — the marker would look freshly written"; CONT=$restore; return 1; }
  # The safety catch painted green: a live-looking marker over a binary
  # that cannot honor a sentinel. The tell is the UNCHANGED stamp across
  # an observed restart, which is what the leg just demonstrated.
  ok "downgrade over a live tree: the marker survives with an UNCHANGED boot stamp — the agent's rollback tell"
  local ver
  ver=$(printf '%s' "$c2" | jq -r '.syncer_version')
  ok "the stale marker still advertises verbs from syncer $ver, over a binary that has none — this is why the stamp is the tell"
  CONT=$restore
  return 0
}

# ── run ──────────────────────────────────────────────────────────────
gw_healthy 60 || { echo "gateway never healthy"; exit 1; }
mcx mc alias set m http://minio.flint-system.svc:9000 drill drillsecret > /dev/null

# Rig reset: every leg owns a prefix, and a previous run's debris is a
# previous run's answer. B1's control asserts the manifest is ABSENT,
# which leftover state would silently satisfy in the wrong direction.
for i in $(seq -w 1 25); do
  mcx mc rm --recursive --force --versions "m/$BUCKET/tenants/b$i/" > /dev/null 2>&1
done
# The legs that need more than one prefix of their own: B11a's second
# workspace, B14's three arms, B25's second storm.
for p in b11a b11a2 b14a b14b b14c b25b; do
  mcx mc rm --recursive --force --versions "m/$BUCKET/tenants/$p/" > /dev/null 2>&1
done
$K exec verbs-a -c sync -- /bin/sh -c 'rm -rf /work/b*' > /dev/null 2>&1
$K exec verbs-b -c sync -- /bin/sh -c 'rm -rf /work/b*' > /dev/null 2>&1
$K exec verbs-s -c sync -- /bin/sh -c 'rm -rf /work/b*' > /dev/null 2>&1
$K exec verbs-s2 -c sync -- /bin/sh -c 'rm -rf /work/b*' > /dev/null 2>&1
echo "  rig reset: 25 leg prefixes and their workspaces cleared"

leg "B1  sentinel publishes when cadence cannot"     b1_sentinel_beats_cadence
leg "B2  crash between consume and ack re-runs"      b2_crash_between_consume_and_ack
leg "B3  storm coalesces, ack covers every touch"    b3_storm_coalesces_and_covers
leg "B4  stale-ack discrimination"                   b4_stale_ack_is_discriminated
leg "B5  scoped sync defers, never loses"            b5_scoped_sync_defers_out_of_scope
leg "B6  conflict transport rides the ack"           b6_conflict_rides_the_ack
leg "B7  remote.seq is local-only news"              b7_ticker_is_local_only_news
leg "B8  unused verbs cost nothing"                  b8_unused_verbs_cost_nothing
leg "B16 the default floor still publishes"          b16_default_floor_still_publishes
leg "B11a SIGTERM settles an owed ack"               b11a_sigterm_settles_an_owed_ack
leg "B13 a legacy citation survives the upgrade"     b13_legacy_citation_survives_the_upgrade
leg "B14 mixed-fleet detection, both ways"           b14_mixed_fleet_is_detectable_both_ways
leg "B17 renewal survives a sentinel storm"          b17_renewal_survives_a_sentinel_storm
leg "B18 deposed mid-pending refuses"                b18_deposed_mid_pending_refuses
leg "B22 pre-existing .flint/ disables the verbs"    b22_preexisting_flint_disables_the_verbs
leg "B25 hot loops meter by work, not by calls"      b25_hot_loops_meter_by_work_not_by_calls

# The reconciliation. Skipped under -only, which runs one leg on
# purpose; a full run that does not match its roster is a failure, not
# a note.
if [ -z "$ONLY" ]; then
  acct_missing=""
  for want in $EXPECTED_LEGS; do
    case " $RAN_LEGS " in *" $want "*) ;; *) acct_missing="$acct_missing $want";; esac
  done
  acct_extra=""
  for got in $RAN_LEGS; do
    case " $EXPECTED_LEGS " in *" $got "*) ;; *) acct_extra="$acct_extra $got";; esac
  done
  if [ -n "$acct_missing" ]; then
    echo "  BAD: the roster declares legs that never ran:$acct_missing"
    FAILED=$((FAILED + 1))
  fi
  if [ -n "$acct_extra" ]; then
    echo "  BAD: legs ran that the roster does not declare:$acct_extra" \
         "— add them to EXPECTED_LEGS and to the plan's §6 matrix"
    FAILED=$((FAILED + 1))
  fi
  [ -n "$acct_missing$acct_extra" ] || \
    echo "  accounting: $(printf '%s' "$RAN_LEGS" | wc -w | tr -d ' ') legs ran, roster reconciles"
fi

echo
echo "─────────────────────────────────────────────"
echo "boundary-verbs bucket drill: $PASS passed, $FAILED failed, $SKIPPED skipped"
for n in ${NOTES+"${NOTES[@]}"}; do echo "  note: $n"; done
[ "$FAILED" -eq 0 ]
