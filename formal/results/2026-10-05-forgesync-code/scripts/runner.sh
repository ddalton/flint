#!/bin/bash
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; mkdir -p $O; log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
( while true; do aws s3 cp $O/ s3://flint-tlc-forge-20261005/out/ --recursive --quiet; sleep 60; done ) &
cd /opt/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build.log 2>&1 || { log BUILD-FAILED; echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-forge-20261005/out/ --recursive --quiet; shutdown -h +2; exit 1; }
log "tlc-rs built; ForgeSync.tla $(md5sum /opt/flint/formal/ForgeSync.tla | cut -c1-8)"
(cd /opt/flint/formal && tlc-rs/target/release/tlc-rs -codegen /data/warm -config ForgeSyncLive.cfg ForgeSync.tla > /dev/null 2>&1 && cd /data/warm && cargo fetch > /dev/null 2>&1); rm -rf /data/warm
python3 /opt/flint/jobs.py $(( $(date +%s) + 3*3600 )) > $O/jobs.log 2>&1; log "jobs done"
echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-forge-20261005/out/ --recursive --quiet; log DONE
shutdown -h +5
