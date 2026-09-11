#!/usr/bin/env bash
# The PUBLISH arm: how much is on the table for lean's upload path?
#
# `compose_parts_and_complete` (flint-store/src/s3.rs:1033) walks a
# large object's parts in a plain `for` loop, awaiting each
# `upload_part().send()` before starting the next. Parts are 32-wide
# ACROSS objects and strictly serial WITHIN one — the mirror image of
# the read-path defect the ranged GET work just fixed, and the worse of
# the two for checkpointing, which writes often and restores rarely.
#
# Parallelising that is a code change, so there is nothing to A/B yet.
# But `aws s3 cp` already uploads multipart with parallel parts, so it
# measures the HEADROOM on the same node, the same bucket, the same
# bytes, with no lean code touched. If lean's barrier and a 32-wide CLI
# upload come out together, the serial loop is not costing anything and
# the fix is not worth writing. If the CLI is far ahead, that gap is
# what parallel parts would recover.
#
#   L-pub     lean barrier             serial parts within an object
#   S-pub-32  aws s3 cp, 32 concurrent parallel parts — the headroom
#   S-pub-1   aws s3 cp, 1 concurrent  the POSITIVE CONTROL
#
# S-pub-1 runs ONCE PER WORKLOAD, not once per rep. Its job is to show
# the rig can see upload concurrency at all; it is a control, not an
# effect, and it does not need its own confidence interval. The last
# drill spent 57 of its 93 minutes re-running a control that had already
# fired identically twice — that is the lesson, applied.
#
# Each L-pub needs a FRESH workspace: the baseline in `.flint/` records
# what was published, so a second barrier over the same tree uploads
# nothing and times as a triumph.
set -euo pipefail
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_ROOT:=/mnt/nvme/drill}"
: "${FLINT_SYNC_BIN:=$DRILL_ROOT/flint-sync}"
: "${AWS_REGION:=us-west-1}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION"
REPS="${1:-3}"
WORKLOADS="big mixed"
declare -A WANT_BYTES=( [big]=6442450944 [mixed]=4327735296 )
declare -A WANT_FILES=( [big]=6          [mixed]=2001       )
RESULTS="$DRILL_ROOT/results/publish-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p "$DRILL_ROOT/results"

uniq_prefix() { echo "pubarm/$1/$(date -u +%s)-$RANDOM"; }

lean_publish() { # <workload> -> "elapsed_ms"
  local w="$1" src="$DRILL_ROOT/seed-$w" dir="$DRILL_ROOT/pub-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  # Copy OUTSIDE the timed window: this measures the upload, not cp.
  cp -r "$src"/. "$dir"/ 2>/dev/null || true
  rm -rf "$dir/.flint"
  sync; echo 3 > /proc/sys/vm/drop_caches
  local t0 t1
  t0=$(date +%s%N)
  FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
  FLINT_SYNC_PREFIX="$(uniq_prefix "$w")" FLINT_SYNC_FANOUT=32 \
    "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  t1=$(date +%s%N)
  echo "$(( (t1 - t0) / 1000000 ))"
}

cli_publish() { # <workload> <concurrency> -> "elapsed_ms bytes files"
  local w="$1" conc="$2" src="$DRILL_ROOT/seed-$w" pfx
  pfx="$(uniq_prefix "$w")"
  # Set the concurrency BEFORE the clock starts. `aws configure set` is
  # a second CLI invocation — about a second of Python start-up — and
  # inside the window it would be charged to the arm lean is being
  # compared against, which is the direction that flatters lean.
  aws configure set default.s3.max_concurrent_requests "$conc"
  sync; echo 3 > /proc/sys/vm/drop_caches
  local t0 t1
  t0=$(date +%s%N)
  # The control dir is `.flint-sync/`, NOT `.flint/`. An exclude that
  # matches nothing is invisible: the upload simply carries six extra
  # control files, the byte and object counts drift from the seeded
  # tree, and the arm reads as a slightly slow upload rather than as
  # the wrong upload. The guard below is what catches it.
  AWS_MAX_ATTEMPTS=5 aws s3 cp --recursive --quiet "$src" \
    "s3://$DRILL_BUCKET/$pfx" --exclude "*.flint-sync/*" --exclude ".flint-sync/*"
  t1=$(date +%s%N)
  # Read the bytes back off S3, not off the local tree: an upload that
  # silently dropped objects would otherwise report the local total and
  # look like a win.
  local b n
  b=$(aws s3 ls --recursive "s3://$DRILL_BUCKET/$pfx" | awk '{s+=$3} END {print s+0}')
  n=$(aws s3 ls --recursive "s3://$DRILL_BUCKET/$pfx" | wc -l)
  echo "$(( (t1 - t0) / 1000000 )) $b $n"
}

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$@" | tee -a "$RESULTS"; }

echo "results -> $RESULTS" >&2
printf '%-5s %-7s %-10s %9s %12s %7s\n' rep workload arm ms bytes files
for w in $WORKLOADS; do
  read -r ms b n <<<"$(cli_publish "$w" 1)"
  row "ctl" "$w" "S-pub-1" "$ms" "$b" "$n"
done
for rep in $(seq 1 "$REPS"); do
  for w in $WORKLOADS; do
    row "$rep" "$w" "L-pub" "$(lean_publish "$w")" "-" "-"
    read -r ms b n <<<"$(cli_publish "$w" 32)"
    row "$rep" "$w" "S-pub-32" "$ms" "$b" "$n"
  done
done

# ── guards ───────────────────────────────────────────────────────────
fail=0
for w in $WORKLOADS; do
  while read -r arm b n; do
    [ "$b" = "-" ] && continue
    [ "$b" = "${WANT_BYTES[$w]}" ] || { echo "GUARD FAIL [$w/$arm]: S3 holds $b bytes, seeded ${WANT_BYTES[$w]} — an upload that dropped objects is not a fast upload" >&2; fail=1; }
    [ "$n" = "${WANT_FILES[$w]}" ] || { echo "GUARD FAIL [$w/$arm]: S3 holds $n objects, seeded ${WANT_FILES[$w]}" >&2; fail=1; }
  done < <(awk -v w="$w" '$2==w {print $3"\t"$5"\t"$6}' "$RESULTS")
  read -r c p <<<"$(awk -v w="$w" '$2==w && $3=="S-pub-1"{c=$4} $2==w && $3=="S-pub-32"{s+=$4; n++} END{if(n) printf "%d %d", c, s/n}' "$RESULTS")"
  if [ -n "${c:-}" ] && [ "$c" -le $(( ${p:-0} * 2 )) ]; then
    echo "GUARD FAIL [$w/control]: 1-way ${c}ms vs 32-way ${p}ms — this rig cannot see upload concurrency, so its L-pub comparison means nothing" >&2
    fail=1
  fi
done
[ "$fail" = 0 ] && echo "GUARDS PASSED" || { echo "GUARDS FAILED — do not quote these numbers." >&2; exit 1; }
