#!/bin/bash
# The contention matrix on the drill box: 3 rounds x arms A B C, rotated so no arm always runs first.
set -u
B=flint-lean-contention-20260914-4725ad
D=/root/c; mkdir -p $D/bin $D/rig $D/runs
aws s3 cp --quiet --recursive s3://$B/_rig/bin/ $D/bin/
aws s3 cp --quiet --recursive s3://$B/_rig/rig/ $D/rig/
chmod +x $D/bin/* $D/rig/*.sh
(cd $D/bin && sha256sum *) > $D/progress.log
nproc >> $D/progress.log; uname -r >> $D/progress.log
aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
orders=("A B C" "B C A" "C A B")
for n in 1 2 3; do
  for arm in ${orders[$((n-1))]}; do
    echo "start $arm $n $(date -u +%T)" >> $D/progress.log
    aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
    BUCKET=$B BIN_DIR=$D/bin RIG=$D/rig ROOT=$D/runs WRITERS=6 FLOOR=5 LOAD_SECS=300 IDLE_SECS=60 AWS_REGION=us-west-1 \
      bash $D/rig/contention.sh run $arm $n >> $D/run.log 2>&1
    rc=$?
    echo "end $arm $n rc=$rc $(date -u +%T) $(tail -1 $D/run.log)" >> $D/progress.log
    aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
    aws s3 cp --quiet $D/run.log s3://$B/_rig/status/run.log
  done
done
echo "ALL DONE $(date -u +%T)" >> $D/progress.log
aws s3 cp --quiet $D/progress.log s3://$B/_rig/status/progress.log
aws s3 cp --quiet $D/run.log s3://$B/_rig/status/run.log
