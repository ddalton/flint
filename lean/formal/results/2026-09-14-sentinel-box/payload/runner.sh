#!/bin/bash
# flint lean formal: three TLC runs of the barrier-lease sentinel world, in order.
#   c0  control: r2's cfg with the gateway guard removed -- MUST violate (Inv_HITLDurable expected)
#   r1  LeanBarrierLeaseSentinel as committed -- expected to stop at depth ~19; keep the FULL log + trace
#   r2  the same world minus Inv_AckBoundaryCoherent only -- run to exhaustion
set -u
BUCKET="$1"
OUT=/data/out; mkdir -p "$OUT" /data/states
cd /data/payload
sync_out() { aws s3 sync "$OUT" "s3://$BUCKET/out/" --quiet --only-show-errors; }
( while true; do { date -u +%FT%TZ; free -g; df -h /data | tail -1; uptime; } > "$OUT/host.txt"; sync_out; sleep 60; done ) &
( sleep $((715*60)); echo "cap reached $(date -u +%FT%TZ)" > "$OUT/CAP"; sync_out ) &
run() {
  local name=$1 cfg=$2
  date -u +%FT%TZ > "$OUT/$name.start"; sync_out
  java -XX:+UseParallelGC -Xmx16g -XX:MaxDirectMemorySize=40g \
    -Dtlc2.tool.fp.FPSet.impl=tlc2.tool.fp.OffHeapDiskFPSet \
    -cp tla2tools.jar tlc2.TLC -workers 8 -checkpoint 0 -fpmem 0.9 \
    -metadir "/data/states/$name" -config "$cfg" LeanSubtree.tla > "$OUT/$name.log" 2>&1
  local rc=$?
  mkdir -p "$OUT/$name.trace"; mv /data/payload/*_TTrace_* "$OUT/$name.trace/" 2>/dev/null
  echo "$rc $(date -u +%FT%TZ)" > "$OUT/$name.exit"; sync_out
  rm -rf "/data/states/$name"
}
run c0-control SentinelNoAckCoherentHitlOverUncited.cfg
run r1-sentinel LeanBarrierLeaseSentinel.cfg
run r2-noackcoherent SentinelNoAckCoherent.cfg
date -u +%FT%TZ > "$OUT/ALLDONE"; sync_out
shutdown -h now
