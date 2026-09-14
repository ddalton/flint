#!/bin/bash
# Arm D (C + the chunk reaper's listing prefilter), interleaved with C, after the first matrix.
set -u
B=flint-lean-contention-20260914-4725ad; D=/root/c
until grep -q "ALL DONE" $D/progress.log 2>/dev/null; do sleep 20; done
aws s3 cp --quiet s3://$B/_rig/bin/flint-sync-D $D/bin/flint-sync-D && chmod +x $D/bin/flint-sync-D
(cd $D/bin && sha256sum flint-sync-D) >> $D/progress.log
for spec in "D 1" "C 4" "D 2" "C 5" "D 3"; do
  set -- $spec
  echo "start $1 $2 $(date -u +%T)" >> $D/progress.log
  aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
  BUCKET=$B BIN_DIR=$D/bin RIG=$D/rig ROOT=$D/runs WRITERS=6 FLOOR=5 LOAD_SECS=300 IDLE_SECS=60 AWS_REGION=us-west-1 \
    bash $D/rig/contention.sh run $1 $2 >> $D/run.log 2>&1
  rc=$?
  echo "end $1 $2 rc=$rc $(date -u +%T) $(tail -1 $D/run.log)" >> $D/progress.log
  aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
done
echo "MORE DONE $(date -u +%T)" >> $D/progress.log
aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
aws s3 cp --quiet $D/run.log s3://$B/_rig/status/run.log
