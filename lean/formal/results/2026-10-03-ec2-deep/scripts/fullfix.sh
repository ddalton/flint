#!/bin/bash
# The full deep run on the FIXED build (per-thread constant pool), 32 workers.
O=/data/out; log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
export AWS_DEFAULT_REGION=us-east-1
G=/opt/fix/tgt-LeanP1Holds/release/tlcgen-leanp1; M=/opt/payload/model
[ "$(md5sum $M/LeanP1.tla | cut -c1-32)" = 94da754147dd4a821bc32d81d5f923b9 ] || { log "FULLFIX MD5 CHANGED"; exit 4; }
pgrep -f "while true; do sleep 120" >/dev/null || ( while true; do sleep 120; { date -u +%FT%TZ; grep -E 'MemAvailable' /proc/meminfo; df -h /data | tail -1; uptime; } >> $O/sys.log; aws s3 cp $O/ s3://flint-tlc-deep-20261003/out/ --recursive --quiet; done ) &
rm -rf /data/md-fullfix
log "FULLFIX start: fixed build, 32 workers, -fpmem 48000 -queue-mem 80000 -checkpoint 0"
cd $M && $G -workers 32 -fpmem 48000 -queue-mem 80000 -checkpoint 0 -metadir /data/md-fullfix -config LeanP1Holds.cfg LeanP1.tla > $O/fullfix.out 2>&1
rc=$?
log "FULLFIX rc=$rc | $(grep -E 'No error has been found|is violated|^Error' $O/fullfix.out | head -1) | $(grep -E 'distinct states found' $O/fullfix.out | tail -1)"
echo FULLFIX-DONE >> $O/runner.log
aws s3 cp $O/ s3://flint-tlc-deep-20261003/out/ --recursive --quiet
