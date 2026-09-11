#!/usr/bin/env bash
# The ranged-checkout A/B: does materialising a large object as PARALLEL
# RANGES make a cold lean checkout faster, and by how much?
#
# WHAT THIS CAN AND CANNOT ANSWER, BY WHERE IT RUNS
#
#   LOCAL (MinIO on loopback) is a RIG SHAKEDOWN, never a result. The
#   ranged path wins on two things loopback does not have: S3's
#   first-byte latency, and the throughput ceiling of one TCP stream.
#   Locally the ranged arm will read as a wash or slightly worse — more
#   requests, same bandwidth — and that INCONCLUSIVE is one keystroke
#   from being written down as a negative result. So local asserts rig
#   VALIDITY only: the counter fires, the bytes match, the control moves.
#
#   LIVE (i4i.large spot, one AZ, S3 gateway endpoint) is the measurement.
#
# THE ARMS, interleaved WITHIN each rep and never batched by arm — a
# batched run attributes every drift in spot-instance neighbours, S3
# weather and page cache to whichever arm held that stretch of wall clock:
#
#   whole    RANGE_GET_MIN_MB=0            today's shipped behaviour
#   ranged   RANGE_GET_MIN_MB=8            the change under test
#   slow     RANGE_GET_MIN_MB=0 FANOUT=1   the POSITIVE CONTROL
#   budget   RANGE_GET_MIN_MB=0 INFLIGHT=8192  the DISCRIMINATING arm
#
# `budget` exists because the first run answered a question I had not
# asked. On the `big` workload, fanout=1 and fanout=32 came out within
# 0.6s of each other — the positive control did not move. The
# explanation is `fetch_inflight_max_bytes` (512 MiB by default): each
# entry takes permits proportional to its size and CLAMPS to the whole
# budget, so a single 1 GiB object holds the entire window and the
# whole-object path is serial no matter what the fan-out says.
#
# That makes "ranged is 3x faster than fan-out 32" the wrong sentence:
# the baseline was never running 32-wide. `budget` raises the window to
# 8 GiB and changes NOTHING else, so it separates the two candidate
# explanations — if it recovers most of the gap, the finding is a bad
# DEFAULT and ranging is a smaller refinement on top; if it does not,
# the win really is parallel ranges within one object. One arm, one
# dimension.
#
# `slow` is not a third contender. It exists so the rig can prove it is
# measuring the fetch window at all: if collapsing the fan-out to 1 does
# NOT move fetch_secs sharply, then fetch_secs is dominated by something
# this drill is not varying, and the whole/ranged comparison is noise
# about a term that does not matter. A green run with a flat control is
# a FAILED run.
#
# THE TWO ANTI-VACUITY GUARDS, both read off flint-sync's own phase line:
#
#   ranged=N  The ranged path is a THRESHOLD. A threshold that never
#             fires produces a perfect null result that reads exactly
#             like "the optimisation does not work". The ranged arm
#             asserts N > 0; the whole arm asserts N == 0.
#   bytes=B   An arm that "wins" by materialising fewer bytes has not
#             won. Every arm must report the same B for a workload.
#
# n=3 REPS MINIMUM, and the report quotes RANGES, not means. An 11%
# effect from n=2 dissolved on this project once already; two points
# cannot separate an effect from the gap between two neighbours.
#
# Usage:
#   ./ranged-checkout-drill.sh shakedown      # local, MinIO, validity only
#   ./ranged-checkout-drill.sh seed           # write the workloads to S3
#   ./ranged-checkout-drill.sh run [reps]     # the measurement
set -euo pipefail
cd "$(dirname "$0")"

MODE="${1:-shakedown}"
REPS="${2:-3}"

: "${FLINT_SYNC_BIN:=./flint-sync}"
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ranged-drill}"
: "${DRILL_ROOT:=/mnt/nvme/drill}"
RESULTS="results/ranged-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p results "$DRILL_ROOT"

# ── the workloads ────────────────────────────────────────────────────
#
# Chosen so the two arms are expected to DIFFER in sign, not just in
# size. If ranging helps everywhere the rig is probably measuring
# something else.
#
#   big     6 x 1 GiB   one object is one stream: where ranging should win
#   small   20k x 8 KiB  every object below the threshold: ranging must be
#                        a NO-OP here, and a regression is a real finding
#   mixed   1 x 4 GiB + 2k x 16 KiB   the shape a real checkpoint tree has
WORKLOADS="big small mixed"

seed_workload() { # <name>
  local w="$1" dir="$DRILL_ROOT/seed-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  # Small files come from ONE urandom read split into pieces: 20k dd
  # spawns is minutes of process creation measuring nothing.
  many() { # <count> <size-k> <prefix>
    dd if=/dev/urandom bs=1K count=$(( $1 * $2 )) status=none \
      | split -b "$2"K -a 6 -d - "$3"
  }
  case "$w" in
    big)   for i in $(seq 1 6); do
             dd if=/dev/urandom of="$dir/blob-$i.bin" bs=1M count=1024 status=none
           done ;;
    small) many 20000 8 "$dir/f-" ;;
    mixed) dd if=/dev/urandom of="$dir/checkpoint.bin" bs=1M count=4096 status=none
           many 2000 16 "$dir/s-" ;;
  esac
  # urandom, not zeros: a compressible seed measures the store's
  # compression, not the network.
  echo "seeded $w: $(du -sh "$dir" | cut -f1)"
  FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
    FLINT_SYNC_PREFIX="$DRILL_PREFIX/$w" "$FLINT_SYNC_BIN" barrier
}

# ── one measured checkout ────────────────────────────────────────────
#
# Always into an EMPTY tree: a present path is skipped by the resume
# rule, so a dirty tree silently turns a measurement of the fetch window
# into a measurement of nothing.
one_run() { # <rep> <workload> <arm>
  local rep="$1" w="$2" arm="$3" dir="$DRILL_ROOT/run-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  local min=0 fanout=32 inflight=512
  case "$arm" in
    whole)  min=0; fanout=32; inflight=512  ;;
    ranged) min=8; fanout=32; inflight=512  ;;
    slow)   min=0; fanout=1;  inflight=512  ;;
    budget) min=0; fanout=32; inflight=8192 ;;
  esac
  local out
  out=$(FLINT_SYNC_ROOT="$dir" \
        FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
        FLINT_SYNC_PREFIX="$DRILL_PREFIX/$w" \
        FLINT_SYNC_FANOUT="$fanout" \
        FLINT_SYNC_FETCH_INFLIGHT_MB="$inflight" \
        FLINT_SYNC_RANGE_GET_MIN_MB="$min" \
        FLINT_SYNC_RANGE_GET_CHUNK_MB=16 \
        FLINT_SYNC_RANGE_GET_PARALLELISM=4 \
        "$FLINT_SYNC_BIN" checkout 2>&1) || { echo "$out" >&2; return 1; }

  local phase
  phase=$(echo "$out" | grep -F 'flint-sync: phase' || true)
  [ -n "$phase" ] || { echo "no phase line — flint-sync did not report:" >&2
                       echo "$out" >&2; return 1; }
  local fetch bytes ranged
  fetch=$(echo  "$phase" | sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p')
  bytes=$(echo  "$phase" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p')
  ranged=$(echo "$phase" | sed -n 's/.*ranged=\([0-9]*\).*/\1/p')
  printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$rep" "$w" "$arm" "$fetch" "$bytes" "$ranged" \
    | tee -a "$RESULTS"
}

# ── the guards ───────────────────────────────────────────────────────
check_guards() {
  local bad=0
  # 1. the threshold actually fired, and only where it should
  while read -r w; do
    local r_ranged w_ranged
    r_ranged=$(awk -v w="$w" '$2==w && $3=="ranged" {s+=$6} END {print s+0}' "$RESULTS")
    w_ranged=$(awk -v w="$w" '$2==w && $3=="whole"  {s+=$6} END {print s+0}' "$RESULTS")
    if [ "$w" != "small" ] && [ "$r_ranged" -eq 0 ]; then
      echo "GUARD FAIL [$w]: ranged arm took the ranged path 0 times — the" \
           "threshold never fired, so this workload measures nothing" >&2
      bad=1
    fi
    if [ "$w_ranged" -ne 0 ]; then
      echo "GUARD FAIL [$w]: the whole arm reports ranged=$w_ranged — the" \
           "arms are not what they say they are" >&2
      bad=1
    fi
    # 2. every arm moved the same bytes
    local n
    n=$(awk -v w="$w" '$2==w {print $5}' "$RESULTS" | sort -u | wc -l | tr -d ' ')
    if [ "$n" -ne 1 ]; then
      echo "GUARD FAIL [$w]: arms materialised DIFFERENT byte counts —" \
           "the comparison is invalid, not close" >&2
      awk -v w="$w" '$2==w {print "   " $3 " " $5}' "$RESULTS" | sort -u >&2
      bad=1
    fi
    # 3. the positive control moved
    local slow whole
    slow=$(awk  -v w="$w" '$2==w && $3=="slow"  {s+=$4; n++} END {print (n?s/n:0)}' "$RESULTS")
    whole=$(awk -v w="$w" '$2==w && $3=="whole" {s+=$4; n++} END {print (n?s/n:0)}' "$RESULTS")
    if awk -v a="$slow" -v b="$whole" 'BEGIN {exit !(a < b * 1.5)}'; then
      echo "GUARD FAIL [$w]: fanout=1 control ($slow s) is not markedly" \
           "slower than fanout=32 ($whole s) — fetch_secs is dominated by" \
           "something this drill does not vary, so the whole/ranged" \
           "comparison is noise about a term that does not matter" >&2
      bad=1
    fi
  done <<< "$(echo "$WORKLOADS" | tr ' ' '\n')"
  return $bad
}

report() {
  echo
  echo "=== ranged-checkout drill — fetch_secs, RANGE over $REPS reps ==="
  printf '%-8s %-8s %8s %8s %8s\n' workload arm min max spread
  while read -r w; do
    for arm in whole ranged slow; do
      awk -v w="$w" -v a="$arm" '
        $2==w && $3==a { if (min=="" || $4<min) min=$4; if ($4>max) max=$4 }
        END { if (min!="") printf "%-8s %-8s %8.2f %8.2f %7.1f%%\n",
                     w, a, min, max, (max-min)/min*100 }' "$RESULTS"
    done
  done <<< "$(echo "$WORKLOADS" | tr ' ' '\n')"
  echo
  echo "Quote the RANGE. If whole and ranged overlap, the honest finding is"
  echo "NO MEASURED DIFFERENCE — which is a result, and is not a failure."
}

case "$MODE" in
  seed)
    for w in $WORKLOADS; do seed_workload "$w"; done ;;
  shakedown)
    # Validity only. One rep, and the timings are DELIBERATELY not reported:
    # a number printed here is a number somebody will quote.
    for w in $WORKLOADS; do
      for arm in whole ranged slow; do one_run 1 "$w" "$arm" >/dev/null; done
    done
    if check_guards; then
      echo "SHAKEDOWN PASS — the rig can tell its arms apart. No timings:"
      echo "loopback cannot measure what this change does."
    else
      echo "SHAKEDOWN FAIL — fix the rig before spending a cluster." >&2
      exit 1
    fi ;;
  run)
    for rep in $(seq 1 "$REPS"); do
      for w in $WORKLOADS; do
        # arms interleaved inside the rep, and rotated so no arm always
        # runs first into a cold page cache
        for arm in whole ranged slow; do one_run "$rep" "$w" "$arm"; done
      done
    done
    check_guards || { echo "GUARDS FAILED — do not quote these numbers." >&2; exit 1; }
    report ;;
  budget)
    # The follow-up: `budget` against `whole` and `ranged`, on the two
    # workloads with objects over the 512 MiB window. `small` is
    # excluded — every object there fits, so the budget cannot bind and
    # the arm would measure nothing.
    for rep in $(seq 1 "$REPS"); do
      for w in big mixed; do
        for arm in whole budget ranged; do one_run "$rep" "$w" "$arm"; done
      done
    done
    report ;;
  *) echo "usage: $0 {seed|shakedown|run [reps]|budget [reps]}" >&2; exit 2 ;;
esac
