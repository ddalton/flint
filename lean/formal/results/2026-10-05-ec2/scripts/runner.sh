#!/bin/bash
# 2026-10-05 run on the c8g: build tlc-rs (99beb8d0), then jobs.py, then the conditional TLC script; results to s3://flint-tlc-run-20261005/out/ every 60 s.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; mkdir -p $O /data/gt /data/gen /data/st
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
( while true; do aws s3 cp $O/ s3://flint-tlc-run-20261005/out/ --recursive --quiet; sleep 60; done ) &
DL=$(( $(date +%s) + 9*3600 ))
cd /opt/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build.log 2>&1 || { log TLCRS-BUILD-FAILED; echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-run-20261005/out/ --recursive --quiet; shutdown -h +5; exit 1; }
log "tlc-rs built at 99beb8d0"
python3 /opt/flint/jobs.py $DL > $O/jobs.log 2>&1; log "jobs done"
bash /opt/flint/tlcscale.sh $DL > $O/tlcscale.log 2>&1; log "tlc step done"
echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-run-20261005/out/ --recursive --quiet; log DONE
shutdown -h +5
