#!/usr/bin/env bash
# ab-parts.sh — FLINT_SYNC_UPLOAD_PART_PARALLELISM on the publish of `mixed` (one 4 GiB object + 2,000 small):
# the shape whose critical path is ONE large object. Arms 1/4/8/16 interleaved per rep, fresh prefix per leg,
# cache dropped before each, landed set checked by LISTING. Runs on the node as root after the write run.
set -uo pipefail
: "${BUCKET:?}"; ROOT=${ROOT:-/mnt/nvme/drill}; BIN=${BIN:-/mnt/nvme/rig/flint-sync}; REPS=${REPS:-2}
export AWS_REGION=us-west-1 AWS_DEFAULT_REGION=us-west-1
TS=$(date -u +%Y%m%d-%H%M%S); OUT=$ROOT/results/ab-parts-$TS.tsv
seed=$ROOT/seed-mixed
for rep in $(seq 1 "$REPS"); do
  for p in 1 4 8 16; do
    rm -rf "$seed/.flint-sync"; sync; echo 3 > /proc/sys/vm/drop_caches
    pfx="ab-parts-p$p-r$rep-$TS/mixed"
    t0=$(( $(date +%s%N) / 1000000 ))
    out=$(FLINT_SYNC_UPLOAD_PART_PARALLELISM=$p FLINT_SYNC_ROOT="$seed" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$pfx" "$BIN" barrier 2>&1); rc=$?
    t1=$(( $(date +%s%N) / 1000000 ))
    rm -rf "$seed/.flint-sync"
    read -r n b <<<"$(aws s3 ls --recursive --summarize "s3://$BUCKET/$pfx/files/" 2>/dev/null | awk '/Total Objects:/ {n=$3} /Total Size:/ {b=$3} END {printf "%d %d", n+0, b+0}')"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$rep" "$p" "$(( t1 - t0 ))" "$rc" "$n" "$b" | tee -a "$OUT"
    echo "$(date -u +%H:%M:%S)   p=$p rep=$rep: $(grep -F 'flint-sync: barrier' <<<"$out" | tail -1)" >&2
    [ "$n" = 2001 ] && [ "$b" = 4327735296 ] || echo "GUARD FAIL p=$p rep=$rep: listed $n files / $b bytes" >&2
  done
done
echo "=== part parallelism on mixed, wall ms per leg (rep, p, ms, rc, files, bytes) ==="; cat "$OUT"
aws s3 cp "$OUT" "s3://$BUCKET/_rig/results/$(basename "$OUT")" --quiet 2>/dev/null || true
