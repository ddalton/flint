#!/usr/bin/env bash
# The small-file ceiling: is ~3,300 files/s on i4i.large a limit of the
# node, of S3, or of ONE THREAD?
#
# WHAT WAS KNOWN GOING IN. Small-file checkout (20k x 8 KiB) plateaued at
# ~3,300 files/s from fanout 128 to 512 at 1.4 of 2 cores; sockets
# scaled with fanout while effective concurrency saturated near 95;
# futex dominated the syscall profile; thirteen mechanisms proposed from
# inspection died on measurement. Nobody had attributed CPU PER THREAD.
#
# WHAT THE LOCAL RIG FOUND (2026-09-12, 2-vCPU Linux VM, fakes3, tmpfs):
# `materialize()` drove every fetch future from ONE `buffer_unordered`
# task on the `#[tokio::main]` block_on thread, so every request's SDK
# work — build, sign, orchestrate, parse, collect, checksum — ran on one
# thread whatever `fanout` said. Per-thread CPU: the main thread at
# 2.0-2.25 s of a 2.5 s fetch window (saturated), every other thread
# combined under 1.2 s. Spawning each fetch as its own task moved the
# main thread to 0.16 s and the work onto the workers: +15-20% on a box
# with 0.7 idle cores, and it exposed a parent-mkdir race that lost one
# file in 5 of 12 runs (fixed, pinned by a test that fails on the old
# walk).
#
# THE QUESTION THIS DRILL ANSWERS, on real S3 with TLS and a real NVMe:
# with the serial task gone, does files/s rise with fanout past 128, and
# to what? The node must have CORES for that to be visible at all — on
# a 2-vCPU node the fix can only buy the idle 0.6 core.
#
#   ARMS (interleaved within each rep, never batched by arm)
#     base   the shipped binary                    FLINT_SYNC_BASE
#     spawn  each fetch on its own task            FLINT_SYNC_SPAWN
#     slow   base at fanout 8 — the POSITIVE CONTROL that fetch_secs
#            moves with fanout at all (run once, first rep only)
#
#   GUARDS, decided before the run so the result cannot be read to taste
#     bytes=B     identical across arms and fanouts, or the arms did
#                 different work
#     ranged=0    every object is below the ranged threshold
#     main_share  the spawn arm's main-thread CPU must be < 25% of the
#                 base arm's. This is the MECHANISM'S OWN FINGERPRINT: a
#                 spawn binary whose main thread is still hot is the
#                 wrong binary, and the timings above it mean nothing.
#     declined    (multi) each syncer must decline exactly the citations
#                 outside its scope, or the shards overlapped and the
#                 aggregate is counting the same file twice
#
#   OUTCOMES
#     spawn flat at 128/256/512, near base   the next ceiling is not
#                                            CPU-serial: look at S3
#                                            (per-connection latency
#                                            under load) and the NIC
#                                            (ethtool -S allowance
#                                            counters, sampled below)
#     spawn rises 128 -> 256 -> 512          fanout was dead code
#                                            behind the serial task;
#                                            in-flight need is
#                                            throughput x RTT
#     spawn rises then flattens              quote the knee and the
#                                            per-thread table at it
#
# n=3 REPS MINIMUM; quote RANGES, never means.
#
# USAGE (on the node, as in ranged-checkout-drill.sh):
#   ./fanout-ceiling-drill.sh seed            # 20 dirs x 1,000 x 8 KiB
#   ./fanout-ceiling-drill.sh single [reps]   # base vs spawn x 128/256/512
#   ./fanout-ceiling-drill.sh multi  [reps]   # 1/2/4 syncers, disjoint subtrees
set -euo pipefail
cd "$(dirname "$0")"

MODE="${1:-single}"
REPS="${2:-3}"
: "${FLINT_SYNC_BASE:=./flint-sync-base}"
: "${FLINT_SYNC_SPAWN:=./flint-sync-shard}"
: "${FLINT_SYNC_SHARD_MI:=./flint-sync-shard-mi}"
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ceiling-drill}"
: "${DRILL_ROOT:=/mnt/nvme/ceiling}"
FANOUTS="${FANOUTS:-128 256 512}"
RESULTS="results/ceiling-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p results "$DRILL_ROOT"
IFACE=$(ip route show default | awk '{print $5; exit}')

# The tree has SUBDIRECTORIES on purpose: the multi leg shards by path
# component (FLINT_SYNC_CHECKOUT_SCOPE matches components, not string
# prefixes — a flat `f-000000` corpus cannot be sharded), and the
# parent-mkdir race the spawn arm exposed is only reachable on a tree
# with a directory to create.
seed_workload() {
  local dir="$DRILL_ROOT/seed-tree"
  rm -rf "$dir"; mkdir -p "$dir"
  for d in $(seq -f 'd%02g' 0 19); do
    mkdir -p "$dir/$d"
    for i in $(seq -f "$d/f-%04g" 0 999); do head -c 8192 /dev/urandom > "$dir/$i"; done
  done
  echo "seeded tree: $(find "$dir" -type f | wc -l) files, $(du -sh "$dir" | cut -f1)"
  local out
  out=$(FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
        FLINT_SYNC_PREFIX="$DRILL_PREFIX/tree" "$FLINT_SYNC_BASE" barrier 2>&1)
  local up parked
  up=$(echo "$out" | sed -n 's/.* up=\([0-9]*\).*/\1/p' | tail -1)
  parked=$(echo "$out" | sed -n 's/.* parked=\([0-9]*\).*/\1/p' | tail -1)
  if [ "${parked:-0}" -ne 0 ] || [ "${up:-0}" -ne 20000 ]; then
    echo "SEED GUARD FAIL: up=${up:-?} parked=${parked:-?} — this prefix already holds objects" >&2
    echo "$out" >&2; exit 1
  fi
  echo "seeded: up=$up parked=$parked"
}

# Per-thread CPU from /proc, sampled until the process exits; the last
# complete sample wins. tid == pid is the main (block_on) thread.
sample_threads() { # <pid> <outfile>
  local pid="$1" out="$2" last="" s
  while kill -0 "$pid" 2>/dev/null; do
    # A thread can vanish between the glob and the read: that awk fails,
    # and under `set -e` a failed `s=$(...)` would kill this sampler with
    # status 2 and the `wait` on it would kill the DRILL — silently, after
    # the first run, which is exactly how the first live attempt died.
    s=$(for t in /proc/"$pid"/task/*/stat; do awk '{print $1, $14+$15}' "$t" 2>/dev/null || true; done) || true
    [ -n "$s" ] && last="$s"
    sleep 0.2
  done
  echo "$last" > "$out"
}
main_and_others() { # <pid> <file> -> "main_cpu others_cpu nthreads"
  awk -v pid="$1" '{c=$2/100; if($1==pid) m=c; else {o+=c; n++}} END{printf "%.2f %.2f %d", m, o, n+1}' "$2"
}

one_run() { # <rep> <arm> <fanout> [scope] [root] -> appends a TSV row, echoes files/s
  local rep="$1" arm="$2" fanout="$3" scope="${4:-}" dir="${5:-$DRILL_ROOT/run}"
  # ARMS: base = shipped binary; shardN = the sharded-driver binary with
  # FLINT_SYNC_FETCH_DRIVERS=N (N=1 is one driver on a worker, N=cores is
  # the design, N=fanout is the spawn-per-fetch shape — one fetch per
  # task — which lost to base on a 6-vCPU loopback rig at 3x the CPU).
  # A `-mi` suffix selects the same sharded binary linked against
  # mimalloc: the shipped binaries are static musl, whose malloc holds
  # ONE lock, and on a 6-vCPU rig six musl drivers burned 3x the CPU of
  # one and went slower, while six mimalloc drivers did 2.2x base's
  # files/s on less CPU than base.
  local bin="$FLINT_SYNC_BASE" drivers=1
  case "$arm" in
    base|slow) ;;
    shard*-mi) bin="$FLINT_SYNC_SHARD_MI"; drivers="${arm#shard}"; drivers="${drivers%-mi}"; [ "$drivers" = F ] && drivers="$fanout" ;;
    shard*) bin="$FLINT_SYNC_SPAWN"; drivers="${arm#shard}"; [ "$drivers" = F ] && drivers="$fanout" ;;
  esac
  rm -rf "$dir"; mkdir -p "$dir"
  sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
  local rx0 rx1; rx0=$(awk -v i="$IFACE:" '$1==i {print $2}' /proc/net/dev)
  local socks=0 errf="$dir.err"
  FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$DRILL_PREFIX/tree" \
    FLINT_SYNC_FANOUT="$fanout" FLINT_SYNC_CHECKOUT_SCOPE="$scope" \
    FLINT_SYNC_FETCH_DRIVERS="$drivers" \
    "$bin" checkout >/dev/null 2>"$errf" &
  local pid=$!
  sample_threads "$pid" "$dir.threads" &
  local sampler=$!
  while kill -0 "$pid" 2>/dev/null; do
    local n; n=$(ss -Htn state established '( dport = :443 )' 2>/dev/null | wc -l)
    [ "$n" -gt "$socks" ] && socks=$n
    sleep 0.5
  done
  wait "$pid" || { echo "checkout FAILED [$arm/$fanout]:" >&2; cat "$errf" >&2; return 1; }
  wait "$sampler" || true
  rx1=$(awk -v i="$IFACE:" '$1==i {print $2}' /proc/net/dev)
  local phase fetch bytes ranged declined mat cpu
  phase=$(grep -F 'flint-sync: phase' "$errf" | head -1)
  fetch=$(echo "$phase" | sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p')
  bytes=$(echo "$phase" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p')
  ranged=$(echo "$phase" | sed -n 's/.*ranged=\([0-9]*\).*/\1/p')
  mat=$(sed -n 's/.*— \([0-9]*\) materialized.*/\1/p' "$errf" | head -1)
  declined=$(sed -n 's/.*— \([0-9]*\) citations declined.*/\1/p' "$errf" | head -1)
  cpu=$(main_and_others "$pid" "$dir.threads")
  local fps; fps=$(awk -v m="$mat" -v f="$fetch" 'BEGIN{printf "%d", m/f}')
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$rep" "$arm" "$fanout" "${scope:-whole}" "$fetch" "$bytes" "$ranged" "$mat" "${declined:-0}" "$cpu" "$socks" "$((rx1-rx0))" >> "$RESULTS"
  echo "rep=$rep arm=$arm fanout=$fanout scope=${scope:-whole} files/s=$fps fetch=${fetch}s bytes=$bytes materialized=$mat cpu(main others threads)=$cpu socks_peak=$socks rx=$((rx1-rx0))"
}

guards_single() {
  local bad=0
  # every row: ranged == 0, and one bytes value across the whole run
  awk -F'\t' '$4=="whole" && $7!=0 {print "GUARD FAIL: ranged=" $7 " on " $2 "/" $3; f=1} END{exit f}' "$RESULTS" || bad=1
  local nb; nb=$(awk -F'\t' '$4=="whole"{print $6}' "$RESULTS" | sort -u | wc -l)
  [ "$nb" -eq 1 ] || { echo "GUARD FAIL: $nb distinct byte totals across arms — the arms did different work" >&2; bad=1; }
  # the mechanism's fingerprint: spawn's main thread < 25% of base's, per fanout
  for f in $FANOUTS; do
    local mb ms
    mb=$(awk -F'\t' -v f="$f" '$2=="base" && $3==f && $4=="whole"{split($10,c," "); s+=c[1]; n++} END{if(n) printf "%.2f", s/n}' "$RESULTS")
    ms=$(awk -F'\t' -v f="$f" '$2=="shard2-mi" && $3==f && $4=="whole"{split($10,c," "); s+=c[1]; n++} END{if(n) printf "%.2f", s/n}' "$RESULTS")
    if [ -n "$mb" ] && [ -n "$ms" ] && awk -v a="$ms" -v b="$mb" 'BEGIN{exit !(a > 0.25*b)}'; then
      echo "GUARD FAIL: at fanout $f the spawn arm's main thread ($ms s) is not < 25% of base's ($mb s) — wrong binary staged?" >&2; bad=1
    fi
  done
  # the positive control: slow (fanout 8) must be far slower than base at 128
  local s1 b128
  s1=$(awk -F'\t' '$2=="slow"{print $5}' "$RESULTS" | sort -n | head -1)
  b128=$(awk -F'\t' '$2=="base" && $3==128 && $4=="whole"{print $5}' "$RESULTS" | sort -n | tail -1)
  if [ -n "$s1" ] && [ -n "$b128" ] && awk -v a="$s1" -v b="$b128" 'BEGIN{exit !(a < 3*b)}'; then
    echo "GUARD FAIL: fanout=8 ($s1 s) is not >= 3x slower than fanout=128 ($b128 s) — fetch_secs is not measuring the fetch window" >&2; bad=1
  fi
  return $bad
}

report() {
  echo; echo "=== fanout-ceiling drill — files/s RANGE over $REPS reps ($RESULTS) ==="
  printf '%-6s %-6s %-7s %8s %8s %10s %10s %6s\n' arm fanout scope min max main_cpu others socks
  awk -F'\t' '$4=="whole"{k=$2" "$3; v=$8/$5; if(!(k in mn)||v<mn[k])mn[k]=v; if(v>mx[k])mx[k]=v; split($10,c," "); m[k]+=c[1]; o[k]+=c[2]; n[k]++; if($11>s[k])s[k]=$11}
    END{for(k in mn){split(k,a," "); printf "%-6s %-6s %-7s %8d %8d %10.2f %10.2f %6d\n", a[1], a[2], "whole", mn[k], mx[k], m[k]/n[k], o[k]/n[k], s[k]}}' "$RESULTS" | sort -k1,1 -k2,2n
  echo; echo "ethtool allowance counters (a nonzero delta means the NIC, not the client, held the line):"
  ethtool -S "$IFACE" 2>/dev/null | grep -E 'allowance_exceeded' || echo "  (ethtool -S unavailable)"
}

multi_run() { # <rep> <arm> <N>
  local rep="$1" arm="$2" n="$3" per=$((20/n)) pids=() dirs=() i
  # Only the rows THIS run appends are its guard's business: the first
  # live run matched every row of the rep and flagged N=1's row (declined
  # 0) against N=2's expectation.
  local before; before=$(wc -l < "$RESULTS" 2>/dev/null || echo 0)
  local t0; t0=$(date +%s.%N)
  for i in $(seq 0 $((n-1))); do
    local scope; scope=$(seq -f 'd%02g' $((i*per)) $((i*per+per-1)) | paste -sd,)
    local dir="$DRILL_ROOT/multi-$i"; dirs+=("$dir")
    ( one_run "$rep" "$arm" 128 "$scope" "$dir" > "$dir.line" ) &
    pids+=($!)
  done
  for p in "${pids[@]}"; do wait "$p"; done
  local t1; t1=$(date +%s.%N)
  local wall; wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
  local expect=$((20000 - per*1000)) bad=0
  tail -n +"$((before + 1))" "$RESULTS" | awk -F'\t' -v e="$expect" '$4!="whole" && $9!=e {print "GUARD FAIL: " $4 " declined " $9 " != " e; f=1} END{exit f}' || bad=1
  echo "rep=$rep arm=$arm N=$n wall=${wall}s aggregate_files/s=$(awk -v w="$wall" 'BEGIN{printf "%d", 20000/w}') $( [ $bad = 0 ] && echo guards=ok || echo GUARDS=FAILED )"
}

case "$MODE" in
  seed) seed_workload ;;
  single)
    for rep in $(seq 1 "$REPS"); do
      # The positive control ONCE, at fanout 8: it proves fetch_secs
      # tracks the fan-out at all (>= 3x slower than base at 128), and a
      # control does not need its own confidence interval. At fanout 1
      # it would be 20k x RTT = eight minutes a rep.
      [ "$rep" = 1 ] && one_run "$rep" slow 8
      for f in $FANOUTS; do for arm in ${ARMS:-base shard1 shard2 shard2-mi shardF-mi}; do one_run "$rep" "$arm" "$f"; done; done
    done
    guards_single || { echo "GUARDS FAILED — do not quote these numbers." >&2; report; exit 1; }
    report ;;
  multi)
    for rep in $(seq 1 "$REPS"); do
      for n in 1 2 4; do for arm in ${ARMS:-base shard2-mi}; do multi_run "$rep" "$arm" "$n"; done; done
    done ;;
  *) echo "usage: $0 {seed|single [reps]|multi [reps]}" >&2; exit 2 ;;
esac
