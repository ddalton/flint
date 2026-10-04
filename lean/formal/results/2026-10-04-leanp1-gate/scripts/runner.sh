#!/bin/bash
# LeanP1 gate on the c8g: build tlc-rs main, one compiled checker per world, run leangate.py.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; mkdir -p $O /data/gt /data/gen; B=s3://$BUCKET
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
( while true; do aws s3 cp $O/ $B/out/ --recursive --quiet; sleep 60; done ) &
F=/opt/flint/lean/formal
[ "$(md5sum $F/LeanP1.tla | cut -c1-32)" = 94da754147dd4a821bc32d81d5f923b9 ] || { log "MD5 CHANGED"; exit 4; }
cd /opt/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build.log 2>&1 || { log TLCRS-BUILD-FAILED; exit 1; }
T=/opt/flint/formal/tlc-rs/target/release/tlc-rs; log "tlc-rs built"
(cd $F && $T -codegen /data/warm -config LeanP1FetchHolds.cfg LeanP1.tla > /dev/null 2>&1 && cd /data/warm && cargo fetch > /dev/null 2>&1); rm -rf /data/warm
build() { w=$1; m=LeanP1.tla; case $w in LeanP1AllHolds|LeanP1ProbeReaderRescoped|LeanP1ProbeReaderPulledMidRescope) m=MCLeanP1All.tla;; esac
  (cd $F && $T -codegen /data/gen/$w -config $w.cfg $m > /data/gen/$w.log 2>&1) && (cd /data/gen/$w && CARGO_TARGET_DIR=/data/gt/$w cargo build --release >> /data/gen/$w.log 2>&1) || echo "$w build failed" >> $O/builds-failed.txt; }
export -f build; export T F
cut -f1 $F/WORLDS-LeanP1.tsv | grep -vxE 'LeanP1Holds|LeanP1LiveHolds' | xargs -P 40 -I{} bash -c 'build {}'
log "checkers built: $(ls /data/gt | wc -l), failed: $(cat $O/builds-failed.txt 2>/dev/null | wc -l)"
DL=$(( $(date +%s) + 55*60 ))
python3 /opt/flint/leangate.py $DL 300 > $O/leangate.log 2>&1
log "DONE"; aws s3 cp $O/ $B/out/ --recursive --quiet; echo DONE > $O/DONE; aws s3 cp $O/DONE $B/out/DONE --quiet
