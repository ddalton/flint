#!/usr/bin/env bash
# Why is lean slower than mount-s3 on large objects?
#
# The door drill measured the GAP (passthrough ~11 s vs lean-ranged ~22 s
# for 6 x 1 GiB) but not its CAUSE, and there are two candidates that
# predict the same gap:
#
#   (a) THE DISK. Lean writes 6 GiB to NVMe; mount-s3 writes nothing.
#       If the local disk tops out near the 297 MB/s both disk-writing
#       doors achieved, then lean is already at the floor and no amount
#       of extra S3 parallelism moves it.
#
#   (b) THE WINDOW. mountpoint-s3's prefetcher doubles its read window
#       per sequential read up to `max_read_window_size` = 2 GiB, split
#       into 8 MiB parts, and sizes its connection pool from the
#       instance's DETECTED network throughput. Lean's ranged path is
#       fixed at range_get_chunk_bytes (16 MiB) x range_get_parallelism
#       (4) = 64 MiB in flight per object — about 16x less.
#
# Guessing between them is how a plausible story becomes a wrong fix, so
# this runs one experiment per candidate, each varying ONE thing.
#
# ── arm DISK: what can this device actually absorb? ──────────────────
#
# Written the way lean writes: buffered, no per-file fsync, one sync at
# the end (write_via_tmp_fast + sync_tree). The final sync is INSIDE the
# timed window — 6 GiB on a 16 GB node partly lands in page cache, and a
# timer stopped before the sync measures memcpy, not the disk.
#
# `direct` is the positive control: the same bytes with O_DIRECT, which
# cannot be absorbed by page cache at all. If buffered and direct come
# out equal the page cache is not in play; if buffered is wildly faster
# the sync is not landing and the buffered number is not a disk number.
#
# GUARD: zeros must actually consume blocks. A filesystem that stored
# them sparsely would report an enormous write rate for writing nothing,
# which is the same shape as a win.
#
# ── arm WIDE: does lean go faster with more in flight? ───────────────
#
# A 2x2 over the two independent ways lean can have more bytes in
# flight, because they are different fixes with different costs and an
# arm that moves them together cannot say which one paid:
#
#   parallelism 4 -> 16   WITHIN one object (64 -> 256 MiB per object)
#   budget 512 -> 8192    ACROSS objects
#
# The budget leg is the interesting one. Ranged and whole-object fetches
# charge the same permits — `want` is computed from `entry.size` at
# checkout.rs:282, 85 lines before the ranged branch at checkout.rs:368
# decides anything — so a 1 GiB object takes the entire 512 MiB window
# even though a ranged fetch of it never holds more than
# chunk x parallelism = 64 MiB. That is why the six `big` objects run
# strictly one at a time. Raising the budget with ranged ON is a
# CODE-FREE PREVIEW of charging the permit for what the ranged path
# actually uses: same concurrency, and real RSS stays at 6 x 64 MiB.
#
# If (b) is the cause, at least one leg moves. If (a) is the cause, all
# four sit on top of each other. The two candidates disagree, which is
# the point — an experiment both of them pass has separated nothing.
set -euo pipefail
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=ranged-drill}"
: "${DRILL_ROOT:=/mnt/nvme/drill}"
: "${FLINT_SYNC_BIN:=$DRILL_ROOT/flint-sync}"
: "${AWS_REGION:=us-west-1}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION"
REPS="${1:-3}"
cd "$DRILL_ROOT"

disk_write() { # <mode: buffered|direct> <par> -> MB/s
  local mode="$1" par="$2" dir="$DRILL_ROOT/disktest"
  rm -rf "$dir"; mkdir -p "$dir"
  local flag=""; [ "$mode" = direct ] && flag="oflag=direct"
  sync; echo 3 > /proc/sys/vm/drop_caches
  local t0 t1 i
  t0=$(date +%s%N)
  for ((i = 0; i < par; i++)); do
    dd if=/dev/zero of="$dir/w-$i.bin" bs=1M count=$((6144 / par)) \
       status=none $flag &
  done
  wait
  sync                      # inside the window, deliberately
  t1=$(date +%s%N)
  # GUARD: did those bytes really land on the device?
  local used; used=$(du -sm "$dir" | cut -f1)
  if [ "$used" -lt 6000 ]; then
    echo "GUARD FAIL: wrote 6144 MiB but the tree occupies ${used} MiB —" \
         "stored sparsely, so this is a rate for writing nothing" >&2
    return 1
  fi
  awk -v ns=$((t1 - t0)) 'BEGIN { printf "%.0f", 6144 / (ns / 1e9) }'
}

lean_checkout() { # <par> <budget_mb> -> "fetch_s ranged_count"
  local par="$1" budget="$2" dir="$DRILL_ROOT/run-wide"
  rm -rf "$dir"; mkdir -p "$dir"
  sync; echo 3 > /proc/sys/vm/drop_caches
  local out
  out=$(FLINT_SYNC_ROOT="$dir" \
        FLINT_SYNC_BUCKET="$DRILL_BUCKET" \
        FLINT_SYNC_PREFIX="$DRILL_PREFIX/big" \
        FLINT_SYNC_FANOUT=32 \
        FLINT_SYNC_FETCH_INFLIGHT_MB="$budget" \
        FLINT_SYNC_RANGE_GET_MIN_MB=8 \
        FLINT_SYNC_RANGE_GET_CHUNK_MB=16 \
        FLINT_SYNC_RANGE_GET_PARALLELISM="$par" \
          "$FLINT_SYNC_BIN" checkout 2>&1)
  # ranged= is the anti-vacuity guard carried over from the first drill:
  # a threshold that never fired produces a perfect null result that
  # reads exactly like "more parallelism does not help".
  local secs ranged
  secs=$(sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p' <<<"$out")
  ranged=$(sed -n 's/.*ranged=\([0-9]*\).*/\1/p' <<<"$out")
  echo "${secs:-ERR} ${ranged:-0}"
}

echo "=== arm DISK: local NVMe, 6 GiB, lean's write pattern ==="
printf '%-10s %-4s %10s\n' mode par 'MB/s'
for rep in $(seq 1 "$REPS"); do
  for mode in buffered direct; do
    for par in 1 6; do
      printf '%-10s %-4s %10s\n' "$mode" "$par" "$(disk_write "$mode" "$par")"
    done
  done
done

echo
echo "=== arm WIDE: lean ranged on big, 2x2 over in-flight bytes ==="
printf '%-5s %-4s %-9s %9s %8s\n' rep par budget_mb fetch_s ranged
bad=0
for rep in $(seq 1 "$REPS"); do
  for par in 4 16; do
    for budget in 512 8192; do
      read -r secs ranged <<<"$(lean_checkout "$par" "$budget")"
      printf '%-5s %-4s %-9s %9s %8s\n' "$rep" "$par" "$budget" "$secs" "$ranged"
      [ "$ranged" -gt 0 ] 2>/dev/null || { echo "GUARD FAIL [par=$par budget=$budget]: ranged=$ranged — the ranged path never fired, so this row measures nothing" >&2; bad=1; }
    done
  done
done
[ "$bad" = 0 ] || echo "GUARDS FAILED — do not quote the WIDE arm." >&2

echo
echo "READ IT LIKE THIS: if DISK tops out near the ~297 MB/s the"
echo "disk-writing doors achieved, lean is at the floor and WIDE will be"
echo "flat. If DISK is far higher and WIDE moves, the 64 MiB per-object"
echo "window is the constraint and the two range knobs are the fix."
echo "If DISK is high and WIDE is FLAT, it is neither and the next"
echo "suspect is the permit clamp at checkout.rs:282."
