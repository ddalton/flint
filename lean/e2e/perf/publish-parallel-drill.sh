#!/usr/bin/env bash
# Does parallelising a multipart upload's PARTS actually move lean's
# publish, and does it close the gap to a 32-way `aws s3 cp`?
#
# The publish arm measured the gap: lean's barrier took 36.05-37.30 s on
# 6 x 1 GiB and 61.09-61.46 s on 4 GiB + 2k small files, against
# 19.43-19.46 s and 18.99-20.13 s for the CLI on the same node and
# bytes. `compose_parts_and_complete` walked one object's parts in a
# `for` loop, so `fanout` spread work across objects and a tree whose
# critical path is ONE large object got no concurrency at all — which is
# why `mixed` (one 4 GiB object) was the worse of the two.
#
#   L-pub-1   FLINT_SYNC_UPLOAD_PART_PARALLELISM=1   today's behaviour
#   L-pub-8   FLINT_SYNC_UPLOAD_PART_PARALLELISM=8   the change under test
#   S-cli-32  aws s3 cp, 32 concurrent               the HEADROOM reference
#
# `mixed` is the arm that decides this. `big` has six objects, so it
# already gets six-way concurrency from `fanout` and should move less —
# if `big` moves as much as `mixed`, something other than part
# parallelism changed and the result is not what it says it is.
#
# GUARDS: every arm must land the same bytes and the same object count,
# READ BACK FROM S3 rather than counted locally — an upload that
# silently dropped objects is a fast upload by every other measure.
set -euo pipefail
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_ROOT:=/mnt/nvme/drill}"
: "${FLINT_SYNC_BIN:=/mnt/nvme/target/release/flint-sync}"
: "${AWS_REGION:=us-west-1}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION"
REPS="${1:-3}"
WORKLOADS="big mixed"
declare -A WANT_BYTES=( [big]=6442450944 [mixed]=4327735296 )
declare -A WANT_FILES=( [big]=6          [mixed]=2001       )
RESULTS="$DRILL_ROOT/results/pubpar-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p "$DRILL_ROOT/results"

uniq_prefix() { echo "pubpar/$1/$(date -u +%s)-$RANDOM"; }
s3_tally() { aws s3 ls --recursive "s3://$DRILL_BUCKET/$1" | awk '{s+=$3; n++} END {print (s+0)" "(n+0)}'; }

lean_publish() { # <workload> <part_par> -> "ms bytes files"
  local w="$1" par="$2" dir="$DRILL_ROOT/pubpar-$w" pfx
  pfx="$(uniq_prefix "$w")"
  rm -rf "$dir"; mkdir -p "$dir"
  cp -r "$DRILL_ROOT/seed-$w"/. "$dir"/ 2>/dev/null || true
  rm -rf "$dir/.flint-sync"
  sync; echo 3 > /proc/sys/vm/drop_caches
  local t0 t1
  t0=$(date +%s%N)
  FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$pfx" \
  FLINT_SYNC_FANOUT=32 FLINT_SYNC_UPLOAD_PART_PARALLELISM="$par" \
    "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  t1=$(date +%s%N)
  echo "$(( (t1 - t0) / 1000000 )) $(s3_tally "$pfx/files")"
}

cli_publish() { # <workload> -> "ms bytes files"
  local w="$1" pfx; pfx="$(uniq_prefix "$w")"
  aws configure set default.s3.max_concurrent_requests 32
  sync; echo 3 > /proc/sys/vm/drop_caches
  local t0 t1
  t0=$(date +%s%N)
  AWS_MAX_ATTEMPTS=5 aws s3 cp --recursive --quiet "$DRILL_ROOT/seed-$w" \
    "s3://$DRILL_BUCKET/$pfx" --exclude "*.flint-sync/*" --exclude ".flint-sync/*"
  t1=$(date +%s%N)
  echo "$(( (t1 - t0) / 1000000 )) $(s3_tally "$pfx")"
}

row() { printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$@" | tee -a "$RESULTS"; }
printf '%-5s %-7s %-10s %9s %12s %7s\n' rep workload arm ms bytes files
for rep in $(seq 1 "$REPS"); do
  for w in $WORKLOADS; do
    read -r ms b n <<<"$(lean_publish "$w" 1)";  row "$rep" "$w" "L-pub-1"  "$ms" "$b" "$n"
    read -r ms b n <<<"$(lean_publish "$w" 8)";  row "$rep" "$w" "L-pub-8"  "$ms" "$b" "$n"
    read -r ms b n <<<"$(cli_publish "$w")";     row "$rep" "$w" "S-cli-32" "$ms" "$b" "$n"
  done
done

fail=0
for w in $WORKLOADS; do
  while read -r arm b n; do
    [ "$b" = "${WANT_BYTES[$w]}" ] || { echo "GUARD FAIL [$w/$arm]: S3 holds $b bytes, seeded ${WANT_BYTES[$w]}" >&2; fail=1; }
    [ "$n" = "${WANT_FILES[$w]}" ] || { echo "GUARD FAIL [$w/$arm]: S3 holds $n objects, seeded ${WANT_FILES[$w]}" >&2; fail=1; }
  done < <(awk -v w="$w" '$2==w {print $3"\t"$5"\t"$6}' "$RESULTS")
done
[ "$fail" = 0 ] && echo "GUARDS PASSED" || { echo "GUARDS FAILED — do not quote these numbers." >&2; exit 1; }
