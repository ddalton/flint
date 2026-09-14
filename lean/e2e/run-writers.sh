#!/usr/bin/env bash
# The flint-lean WRITERS drill: falsifiers L1–L8 of the per-barrier lease
# (docs/plans/flint-lean-writer-lease-and-gated-assessment.md §6), on a
# cluster. WRITTEN 2026-09-13 beside the implementation and NOT YET RUN —
# every leg below is also pinned locally by a unit test in
# lean/syncer/src/tests.rs (named in each leg), against the in-memory
# store; this drill is what says the same holds against a real bucket,
# real pods and real clocks.
#
# The same rig as run-chaos.sh: kind cluster `flint-lean-chaos` with
# flint-sync:e2e loaded, minio.yaml + chaos.yaml applied. Three writer
# pods (chaos-a, chaos-s, chaos-s2) share ONE prefix per leg; chaos-k is
# the reader. The pods' `sync` container carries the binary; the drill
# execs the verbs with per-leg env, exactly as run-chaos.sh does.
#
# EVERY leg carries an anti-vacuity guard, and where the leg's claim is
# "X did not happen" the guard is that the mechanism that would make X
# happen actually fired (a wait line, a deposal, a preserved copy).
#
# Runtime ~8-10 min: W5 waits out a real 60 s deposal, W7 runs the loop
# for a minute.
set -u
cd "$(dirname "$0")"

CTX=${CTX:-kind-flint-lean-chaos}
K="kubectl --context $CTX"
BUCKET=agentws

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

# ── rig helpers (run-chaos.sh's, verbatim where they overlap) ────────
sy() { # <pod> <prefix> <root> <verb…>
  local pod=$1 prefix=$2 root=$3; shift 3
  $K exec "$pod" -c sync -- /bin/sh -c \
    "FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root /usr/local/bin/flint-sync $*" 2>&1
}
sy_env() { # <pod> <prefix> <root> <extra env> <verb…>
  local pod=$1 prefix=$2 root=$3 extra=$4; shift 4
  $K exec "$pod" -c sync -- /bin/sh -c \
    "FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root $extra /usr/local/bin/flint-sync $*" 2>&1
}
sy_bg() { # <pod> <prefix> <root> <log> <extra env> <verb…>
  local pod=$1 prefix=$2 root=$3 log=$4 extra=$5; shift 5
  $K exec "$pod" -c sync -- /bin/sh -c \
    "nohup env FLINT_SYNC_PREFIX=$prefix FLINT_SYNC_ROOT=$root $extra \
     /usr/local/bin/flint-sync $* > $log 2>&1 & echo bg-started" > /dev/null 2>&1
}
inpod() { local pod=$1; shift; $K exec "$pod" -c sync -- /bin/sh -c "$*" 2>/dev/null; }
mkfiles() { # <pod> <dir> <n> <tag>
  inpod "$1" "mkdir -p $2 && awk -v d=$2 -v n=$3 -v t=$4 \
    \"BEGIN{for(i=1;i<=n;i++){fn=sprintf(\\\"%s/%s%04d.txt\\\", d, t, i); print t \\\"-\\\" i > fn; close(fn)}}\" && echo made"
}
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
mcx()      { $K -n flint-system exec mc -- "$@" 2>/dev/null; }
objcat()   { mcx mc cat "m/$BUCKET/$1"; }
objexists(){ mcx mc stat "m/$BUCKET/$1" > /dev/null 2>&1; }
allkeys()  { mcx mc ls --recursive --json "m/$BUCKET/$1/" | jq -r --arg p "$1/" 'select(.key)|$p + .key'; }
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
cell()     { objcat "$1/.flint/lean/epoch"; }
cell_field() { cell "$1" | jq -r ".$2"; }
writers()  { allkeys "$1/.flint/lean/writers" | grep -c . ; }
dangling() { # <prefix> -> count
  local m present cited
  m=$(manif "$1")
  cited=$(printf '%s' "$m" | jq -r '.entries[].key' | sort)
  present=$(allkeys "$1" | sort)
  comm -23 <(printf '%s\n' "$cited") <(printf '%s\n' "$present") | grep -c .
}
# The conflict ledger a writer keeps (records of what it superseded).
conflicts() { inpod "$1" "cat $2/.flint-sync/conflicts.jsonl 2>/dev/null"; }

# ─────────────────────────────────────────────────────────────────────
# W1  Two writers are both Ready in checkout time and both publish
#     every barrier (L1, L2). Control: run-chaos.sh's old C2 printed
#     `quiet` six times over a minute for the second pod.
#     Local twin: two_writers_publish_without_waiting_for_each_other.
# ─────────────────────────────────────────────────────────────────────
w1_two_writers_both_publish() {
  local P=tenants/w1 R=/work/w1 t0 t1 out
  t0=$(date +%s)
  out=$(sy chaos-a $P $R checkout); has "materialized" "$out" || { bad "A checkout: $out"; return 1; }
  out=$(sy chaos-s $P $R checkout); has "materialized" "$out" || { bad "B checkout: $out"; return 1; }
  t1=$(date +%s)
  if has "quiet" "$out"; then bad "B waited on a fence during checkout: $out"; return 1; fi
  [ $((t1 - t0)) -lt 30 ] || { bad "two checkouts took $((t1 - t0))s"; return 1; }
  ok "both writers checked out at once ($((t1 - t0))s)"
  local i seq_prev=0 seq
  for i in 1 2 3; do
    mkfiles chaos-a $R/a$i 5 a$i > /dev/null
    mkfiles chaos-s $R/b$i 5 b$i > /dev/null
    out=$(sy chaos-a $P $R barrier); has "barrier seq=" "$out" || { bad "A barrier $i: $out"; return 1; }
    out=$(sy chaos-s $P $R barrier); has "barrier seq=" "$out" || { bad "B barrier $i: $out"; return 1; }
    seq=$(manif "$P" | jq -r '.seq')
    [ "$seq" -gt "$seq_prev" ] || { bad "round $i: seq did not advance ($seq_prev -> $seq)"; return 1; }
    seq_prev=$seq
  done
  local cited
  cited=$(manif "$P" | jq -r '.entries|keys|length')
  [ "$cited" -eq 30 ] || { bad "$cited entries cited, want 30 (both writers' work)"; return 1; }
  [ "$(cell_field "$P" released)" = "true" ] || { bad "the fence is held between barriers: $(cell "$P")"; return 1; }
  [ "$(cell_field "$P" epoch)" -eq 6 ] || { bad "epoch $(cell_field "$P" epoch), want 6 (one per barrier)"; return 1; }
  ok "6 barriers, 30 entries cited, fence at rest at epoch 6"
}

# ─────────────────────────────────────────────────────────────────────
# W2  Disjoint edits cross within two barriers (L3).
#     Local twin: disjoint_edits_cross_at_the_next_consume.
# ─────────────────────────────────────────────────────────────────────
w2_disjoint_edits_cross() {
  local P=tenants/w2 R=/work/w2 out
  sy chaos-a $P $R checkout > /dev/null || { bad "A checkout"; return 1; }
  sy chaos-s $P $R checkout > /dev/null || { bad "B checkout"; return 1; }
  inpod chaos-a "mkdir -p $R/a && echo from-A > $R/a/one.txt" > /dev/null
  inpod chaos-s "mkdir -p $R/b && echo from-B > $R/b/one.txt" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "A barrier 1"; return 1; }
  out=$(sy chaos-s $P $R barrier); has "barrier seq=" "$out" || { bad "B barrier 1: $out"; return 1; }
  # B's merge preserved A's entry; B's tree does not have it yet (the
  # merge alone never touches a tree).
  [ -z "$(inpod chaos-s "cat $R/a/one.txt 2>/dev/null")" ] || { bad "the merge wrote into B's tree"; return 1; }
  out=$(sy chaos-s $P $R barrier); has "consumed=1" "$out" || { bad "B's next consume did not integrate A's file: $out"; return 1; }
  [ "$(inpod chaos-s "cat $R/a/one.txt")" = "from-A" ] || { bad "B's tree lacks A's file"; return 1; }
  sy chaos-a $P $R barrier > /dev/null
  out=$(sy chaos-a $P $R barrier); has "consumed=1" "$out" || { bad "A's consume did not integrate B's file: $out"; return 1; }
  [ "$(inpod chaos-a "cat $R/b/one.txt")" = "from-B" ] || { bad "A's tree lacks B's file"; return 1; }
  ok "each writer's file reached the other's tree at its next consume"
}

# ─────────────────────────────────────────────────────────────────────
# W3  A same-path edit is preserved, never lost (L4).
#     Local twin: a_same_path_edit_is_preserved_never_lost.
# ─────────────────────────────────────────────────────────────────────
w3_same_path_preserved() {
  local P=tenants/w3 R=/work/w3 out
  sy chaos-a $P $R checkout > /dev/null || { bad "A checkout"; return 1; }
  inpod chaos-a "mkdir -p $R && echo seed > $R/x.txt" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "A seed barrier"; return 1; }
  sy chaos-s $P $R checkout > /dev/null || { bad "B checkout"; return 1; }
  [ "$(inpod chaos-s "cat $R/x.txt")" = "seed" ] || { bad "B did not check out the seed"; return 1; }
  sleep 1.1  # the scan's mtime clock
  inpod chaos-a "echo A-edit > $R/x.txt" > /dev/null
  inpod chaos-s "echo B-edit-longer > $R/x.txt" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "A barrier"; return 1; }
  out=$(sy chaos-s $P $R barrier); has "parked=0" "$out" || { bad "B parked instead of preserving: $out"; return 1; }
  local key
  key=$(manif "$P" | jq -r '.entries["x.txt"].key')
  [ "$(objcat "$key")" = "B-edit-longer" ] || { bad "the later commit is not current: $(objcat "$key")"; return 1; }
  local rec pk
  rec=$(conflicts chaos-s $R | jq -c 'select(.path=="x.txt" and (.kind|startswith("upload-412-preserved")))' | head -1)
  [ -n "$rec" ] || { bad "B recorded nothing about the version it superseded: $(conflicts chaos-s $R)"; return 1; }
  pk=$(printf '%s' "$rec" | jq -r '.preserved_key')
  [ "$(objcat "$pk")" = "A-edit" ] || { bad "A's bytes are not at the preserved key $pk"; return 1; }
  ok "later commit current; A's bytes preserved at $pk with a record on B"
  sy chaos-a $P $R barrier > /dev/null; sy chaos-a $P $R barrier > /dev/null
  [ "$(inpod chaos-a "cat $R/x.txt")" = "B-edit-longer" ] || { bad "A's clean path did not take B's version"; return 1; }
  ok "A's tree carries the current version after its consume"
}

# ─────────────────────────────────────────────────────────────────────
# W4  Three writers in a hot publish loop: everyone completes every
#     round, nobody reaches the claim deadline (L5, the cluster form;
#     the FIFO itself is pinned by the_ticket_hands_the_fence_to_the_
#     queue_head, whose mutation the drill cannot run).
# ─────────────────────────────────────────────────────────────────────
w4_three_writers_hot_loop() {
  local P=tenants/w4 R=/work/w4 pod ROUNDS=15
  for pod in chaos-a chaos-s chaos-s2; do
    sy $pod $P $R checkout > /dev/null || { bad "$pod checkout"; return 1; }
    inpod $pod "cat > /work/w4-loop.sh <<'EOS'
for i in \$(seq 1 $ROUNDS); do
  mkdir -p $R/\$(hostname) && echo \$i > $R/\$(hostname)/r\$i.txt
  FLINT_SYNC_PREFIX=$P FLINT_SYNC_ROOT=$R /usr/local/bin/flint-sync barrier
done
echo LOOP-DONE
EOS
chmod +x /work/w4-loop.sh" > /dev/null
    $K exec "$pod" -c sync -- /bin/sh -c "nohup /work/w4-loop.sh > /work/w4.log 2>&1 &" > /dev/null 2>&1
  done
  local i done_n
  for i in $(seq 1 120); do
    done_n=0
    for pod in chaos-a chaos-s chaos-s2; do
      has "LOOP-DONE" "$(inpod $pod "cat /work/w4.log")" && done_n=$((done_n + 1))
    done
    [ "$done_n" -eq 3 ] && break
    sleep 2
  done
  [ "$done_n" -eq 3 ] || { bad "only $done_n/3 loops finished in 240 s"; return 1; }
  local waits=0 fails=0 barriers=0
  for pod in chaos-a chaos-s chaos-s2; do
    local log; log=$(inpod $pod "cat /work/w4.log")
    waits=$((waits + $(printf '%s' "$log" | grep -c 'waiting for the publish fence')))
    fails=$((fails + $(printf '%s' "$log" | grep -c 'could not acquire the publish fence')))
    barriers=$((barriers + $(printf '%s' "$log" | grep -c 'barrier seq=')))
  done
  [ "$fails" -eq 0 ] || { bad "$fails barriers reached the claim deadline"; return 1; }
  [ "$barriers" -eq $((ROUNDS * 3)) ] || { bad "$barriers barriers completed, want $((ROUNDS * 3))"; return 1; }
  # ANTI-VACUITY: contention actually happened.
  [ "$waits" -gt 0 ] || { bad "no writer ever waited — the loops never contended; leg vacuous"; return 1; }
  local cited
  cited=$(manif "$P" | jq -r '.entries|keys|length')
  [ "$cited" -eq $((ROUNDS * 3)) ] || { bad "$cited entries cited, want $((ROUNDS * 3))"; return 1; }
  ok "$barriers barriers by 3 writers, $waits contended, 0 deadlines, $cited entries cited"
}

# ─────────────────────────────────────────────────────────────────────
# W5  A holder that stalls INSIDE its commit section is deposed after
#     the quiet polls, its own CAS is fenced, and its next barrier
#     publishes (L6). The window is opened by the drill hold — no drill
#     hits a millisecond commit section by timing.
#     Local twins: a_dead_holder_mid_commit_is_deposed_by_the_next_
#     barrier, a_holder_deposed_mid_commit_abandons_the_barrier.
# ─────────────────────────────────────────────────────────────────────
w5_mid_commit_stall_is_deposed() {
  local P=tenants/w5 R=/work/w5 out
  sy chaos-a $P $R checkout > /dev/null || { bad "A checkout"; return 1; }
  sy chaos-s $P $R checkout > /dev/null || { bad "B checkout"; return 1; }
  inpod chaos-a "mkdir -p $R && echo held > $R/a.txt" > /dev/null
  inpod chaos-s "mkdir -p $R && echo past-a-dead-holder > $R/b.txt" > /dev/null
  local seq0; seq0=$(manif "$P" 2>/dev/null | jq -r '.seq // 0')
  # A: claims, then stalls 150 s inside the commit section.
  sy_bg chaos-a $P $R /work/w5-a.log "FLINT_SYNC_DRILL_HOLD_COMMIT_SECS=150" barrier
  local i
  for i in $(seq 1 60); do
    has "publish fence held" "$(inpod chaos-a "cat /work/w5-a.log")" && break
    sleep 0.5
  done
  has "publish fence held" "$(inpod chaos-a "cat /work/w5-a.log")" || { bad "A never reached its commit section"; return 1; }
  [ "$(cell_field "$P" released)" = "false" ] || { bad "A holds nothing: $(cell "$P")"; return 1; }
  ok "A holds the fence inside its commit section (epoch $(cell_field "$P" epoch))"
  # B: waits out the quiet polls (~60 s), deposes A, rotates, publishes.
  local t0 t1
  t0=$(date +%s)
  out=$(sy chaos-s $P $R barrier)
  t1=$(date +%s)
  has "waiting for the publish fence" "$out" || { bad "B never waited — A held nothing; leg vacuous: $out"; return 1; }
  has "barrier seq=" "$out" || { bad "B's barrier did not complete: $out"; return 1; }
  [ $((t1 - t0)) -ge 50 ] && [ $((t1 - t0)) -le 130 ] || { bad "B's wait was $((t1 - t0))s, want the ~60 s deposal"; return 1; }
  ok "B deposed the stalled holder after $((t1 - t0))s and published"
  manif "$P" | jq -e '.entries["b.txt"]' > /dev/null || { bad "B's file is not cited"; return 1; }
  # A thaws: its CAS is fenced; nothing of its lands; exit non-zero.
  await_exit chaos-a flint-sync 120 || { bad "A never exited"; return 1; }
  local alog; alog=$(inpod chaos-a "cat /work/w5-a.log")
  has "fenced" "$alog" || { bad "the stalled holder did NOT fence: $alog"; return 1; }
  if manif "$P" | jq -e '.entries["a.txt"]' > /dev/null; then bad "the deposed holder's manifest LANDED"; return 1; fi
  ok "A's commit section fenced: $(printf '%s' "$alog" | grep fenced | head -1)"
  # And A is not dead: its next barrier claims again and publishes.
  out=$(sy chaos-a $P $R barrier); has "barrier seq=" "$out" || { bad "A's next barrier failed: $out"; return 1; }
  manif "$P" | jq -e '.entries["a.txt"]' > /dev/null || { bad "A's retry did not cite its file"; return 1; }
  [ "$(dangling "$P")" -eq 0 ] || { bad "dangling citations"; return 1; }
  ok "A's next barrier published; zero dangling"
}

# ─────────────────────────────────────────────────────────────────────
# W6  A reader never touches the fence (L7): a checkout and a sync
#     beside two writers leave the cell byte-for-byte as they found it.
#     Local twin: readers_never_touch_the_cell.
# ─────────────────────────────────────────────────────────────────────
w6_reader_never_claims() {
  local P=tenants/w6 R=/work/w6 out
  sy chaos-a $P $R checkout > /dev/null || { bad "A checkout"; return 1; }
  inpod chaos-a "mkdir -p $R && echo pub > $R/f.txt" > /dev/null
  sy chaos-a $P $R barrier > /dev/null || { bad "A barrier"; return 1; }
  local before after
  before=$(cell "$P")
  out=$(sy chaos-k $P $R checkout); has "materialized" "$out" || { bad "reader checkout: $out"; return 1; }
  out=$(sy chaos-k $P $R sync); has '"status"' "$out" || { bad "reader sync: $out"; return 1; }
  after=$(cell "$P")
  [ "$before" = "$after" ] || { bad "the reader moved the cell: $before -> $after"; return 1; }
  [ "$(inpod chaos-k "cat $R/f.txt")" = "pub" ] || { bad "the reader did not get the file"; return 1; }
  ok "reader checked out and synced; the cell is byte-identical"
}

# ─────────────────────────────────────────────────────────────────────
# W7  The run loop: two writers in `run` for a minute — both publish on
#     the floor, both heartbeats are listed, a SIGTERM drain retires the
#     heartbeat and hands the fence on.
# ─────────────────────────────────────────────────────────────────────
w7_run_loop_heartbeats_and_drain() {
  local P=tenants/w7 R=/work/w7 pod
  for pod in chaos-a chaos-s; do
    sy_bg $pod $P $R /work/w7.log "FLINT_SYNC_FLOOR_SECS=5" run
  done
  sleep 12
  [ "$(writers "$P")" -eq 2 ] || { bad "$(writers "$P") heartbeats listed, want 2"; return 1; }
  ok "two writer heartbeats under .flint/lean/writers/"
  inpod chaos-a "echo a > $R/a.txt" > /dev/null
  inpod chaos-s "echo s > $R/s.txt" > /dev/null
  sleep 14
  local la ls
  la=$(inpod chaos-a "grep -c 'barrier seq=' /work/w7.log"); ls=$(inpod chaos-s "grep -c 'barrier seq=' /work/w7.log")
  [ "${la:-0}" -ge 1 ] && [ "${ls:-0}" -ge 1 ] || { bad "barriers in the loop: A=$la B=$ls"; return 1; }
  manif "$P" | jq -e '.entries["a.txt"] and .entries["s.txt"]' > /dev/null || { bad "both files are not cited"; return 1; }
  ok "both loops published on the floor (A=$la, B=$ls barriers)"
  inpod chaos-a "kill -TERM \$(pidof flint-sync)" > /dev/null
  await_exit chaos-a flint-sync 30 || { bad "A did not exit on SIGTERM"; return 1; }
  has "final drain barrier" "$(inpod chaos-a "cat /work/w7.log")" || { bad "no drain in A's log"; return 1; }
  [ "$(writers "$P")" -eq 1 ] || { bad "$(writers "$P") heartbeats after A's drain, want 1"; return 1; }
  [ "$(cell_field "$P" released)" = "true" ] || { bad "A's drain left the fence held"; return 1; }
  ok "A drained: heartbeat retired, fence handed on; B still listed"
  inpod chaos-s "kill -TERM \$(pidof flint-sync)" > /dev/null
  await_exit chaos-s flint-sync 30 || { bad "B did not exit"; return 1; }
  [ "$(writers "$P")" -eq 0 ] || { bad "heartbeats remain after both drained"; return 1; }
  ok "both drained; no heartbeat remains"
}

# ─────────────────────────────────────────────────────────────────────
leg "W1  two writers both publish, no lifetime wait"        w1_two_writers_both_publish
leg "W2  disjoint edits cross"                              w2_disjoint_edits_cross
leg "W3  same-path edit preserved"                          w3_same_path_preserved
leg "W4  three writers in a hot loop"                       w4_three_writers_hot_loop
leg "W5  a mid-commit stall is deposed and fenced"          w5_mid_commit_stall_is_deposed
leg "W6  a reader never claims"                             w6_reader_never_claims
leg "W7  run loop: heartbeats, floor, drain"                w7_run_loop_heartbeats_and_drain

echo
echo "writers drill: $PASS passed, $FAILED failed"
for n in "${NOTES[@]}"; do echo "  note: $n"; done
[ "$FAILED" -eq 0 ]
