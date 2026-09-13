#!/usr/bin/env bash
# live-size.sh — the LIVE test for atomicity-1 (the manifest cited the scanned size while the upload carried the
# grown file), on runcu against the real bucket. Two arms, each: a syncer daemon (`flint-sync run`, floor 10 s)
# serving an empty prefix; a writer streams a 1 GiB checkpoint into the tree at ~40 MiB/s (~26 s, so two or
# three cadence barriers land mid-write); the moment the first barrier that carries `ckpt.bin` is logged, the
# daemon is SIGKILLed (a spot reclaim: no drain, no chance to re-cite the finished file) and the writer stopped;
# then a SUCCESSOR checks the boundary out into an empty tree with the same binary. Arm OLD is the rig's
# pre-fix binary (head-f7d44444); arm NEW is the fixed one. Expected: OLD's checkout FAILS (the CRC fold over
# [0, cited size) cannot match a longer object); NEW's succeeds and the cited size is the object's.
# The control for the mechanism: the same NEW sequence but with the writer finished BEFORE the first barrier
# (no growth between scan and upload) must also succeed — it must not be the binary that makes checkouts pass.
set -uo pipefail
export AWS_REGION=us-west-1 AWS_DEFAULT_REGION=us-west-1
: "${BUCKET:?}"; ROOT=${ROOT:-/mnt/nvme/drill}; OLD=${OLD:-/mnt/nvme/rig/flint-sync}; NEW=${NEW:-/mnt/nvme/rig/flint-sync-new}
TS=$(date -u +%Y%m%d-%H%M%S)
[ -x "$NEW" ] || { echo "NEW binary missing at $NEW"; exit 2; }
echo "OLD $(sha256sum $OLD | cut -c1-16) NEW $(sha256sum $NEW | cut -c1-16)"

arm() { # <label> <bin> <mode: mid|finished>
  local label=$1 bin=$2 mode=$3
  local pfx="live-size-$TS/$label" w=$ROOT/live-w-$label r=$ROOT/live-r-$label
  rm -rf "$w" "$r"; mkdir -p "$w" "$r"
  local log=$ROOT/live-$label.log
  # The writer's daemon. FLOOR 10 s so barriers land while the file streams.
  env FLINT_SYNC_ROOT="$w" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$pfx" FLINT_SYNC_FLOOR_SECS=10 \
      "$bin" run > "$log" 2>&1 &
  local dpid=$!
  # Wait for the daemon to hold the lease and finish its (empty) checkout.
  for i in $(seq 1 60); do grep -q "holding epoch" "$log" && break; sleep 1; done
  sleep 2
  if [ "$mode" = finished ]; then
    # Control: the whole file is on disk before any barrier can see it.
    head -c 1073741824 /dev/urandom > "$w/ckpt.bin"
    sync
  else
    # Stream at ~30 MiB/s: 1 GiB takes ~35 s; the 10 s floor lands inside it two or three times.
    ( for i in $(seq 1 1024); do head -c 1048576 /dev/urandom >> "$w/ckpt.bin"; sleep 0.024; done ) &
    local wpid=$!
  fi
  # Wait for the FIRST barrier that carries ckpt.bin (the phase line says up=1), then kill the daemon
  # without a drain — and, mid-mode, stop the writer.
  local seen=0
  for i in $(seq 1 120); do
    if grep -E "flint-sync: (phase|barrier).*up=1[^0-9]" "$log" >/dev/null 2>&1; then seen=1; break; fi
    sleep 0.5
  done
  kill -9 "$dpid" 2>/dev/null; wait "$dpid" 2>/dev/null
  [ "$mode" = mid ] && { kill "$wpid" 2>/dev/null; wait "$wpid" 2>/dev/null; }
  local size; size=$(stat -c %s "$w/ckpt.bin" 2>/dev/null || echo 0)
  echo "$label: barrier-with-ckpt seen=$seen; file on disk at kill: $size bytes; daemon log tail:"; grep -E "phase|barrier|error" "$log" | tail -3 | cut -c1-200
  # The successor: a fresh checkout with the same binary. The dead daemon's lease costs the unclean-death lockout.
  local t0=$(date +%s)
  if env FLINT_SYNC_ROOT="$r" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$pfx" "$bin" checkout > "$ROOT/live-$label-checkout.log" 2>&1; then
    echo "$label: CHECKOUT OK in $(( $(date +%s) - t0 )) s; ckpt.bin on the successor: $(stat -c %s "$r/ckpt.bin" 2>/dev/null || echo absent) bytes; object: $(aws s3api head-object --bucket "$BUCKET" --key "$pfx/files/ckpt.bin" --query ContentLength --output text 2>/dev/null) bytes"
  else
    echo "$label: CHECKOUT FAILED in $(( $(date +%s) - t0 )) s: $(grep -iE "fold|crc|error" "$ROOT/live-$label-checkout.log" | tail -2 | cut -c1-220)"
  fi
}

arm old-mid "$OLD" mid
arm new-mid "$NEW" mid
arm new-finished "$NEW" finished
echo "LIVE DONE"
