#!/bin/bash
# 2026-10-05 second c8g: ForgeSyncKeptSetOverlapHolds, then the tlc-rs gate for f00ae156 (main 6e1e7ba2). Results to s3://flint-tlc-gate-20261005/out/.
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; mkdir -p $O; log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
( while true; do aws s3 cp $O/ s3://flint-tlc-gate-20261005/out/ --recursive --quiet; sleep 60; done ) &
G=/opt/gate; T=$G/flint/formal/tlc-rs/target/release/tlc-rs
cd $G/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build.log 2>&1 || { log BUILD-FAILED; echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-gate-20261005/out/ --recursive --quiet; shutdown -h +2; exit 1; }
log "tlc-rs built at 6e1e7ba2, codegen.rs $(md5sum src/codegen.rs | cut -c1-8), symkey.rs $(md5sum src/symkey.rs | cut -c1-8)"
# 1. OverlapHolds (expect HOLDS), compiled, 192 workers, 60 min cap.
K=$G/flint/formal/pending/forge-keptset; w=ForgeSyncKeptSetOverlapHolds
(cd $K && $T -codegen /data/gen-$w -config $w.cfg ForgeSyncKeptSet.tla > $O/$w.build.log 2>&1 && cd /data/gen-$w && CARGO_TARGET_DIR=/data/gt-$w cargo build --release >> $O/$w.build.log 2>&1) && \
 (cd $K && timeout 3600 /data/gt-$w/release/tlcgen-forgesynckeptset -workers 192 -checkpoint 0 -metadir /data/st-$w -fpmem 32000 -queue-mem 60000 -config $w.cfg ForgeSyncKeptSet.tla > $O/$w.out 2>&1; echo "rc=$?" >> $O/$w.out)
log "$w: $(grep -oE 'No error has been found|Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated' $O/$w.out | head -1) | $(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $O/$w.out | tail -1) | $(tail -1 $O/$w.out)"
rm -rf /data/st-$w
# 2. The tlc-rs gate, as 2026-10-03 ran it: the 130 'agree' entries whose cfg has SYMMETRY or VIEW (the only code f00ae156 changes), interpreter AND generated checker, 44 at a time.
(cd $G/flint/formal && $T -codegen /data/warm -config FlintReplication.cfg FlintReplication.tla > /dev/null 2>&1 && cd /data/warm && cargo fetch > /dev/null 2>&1); rm -rf /data/warm
log "gate start"
python3 $G/gate_par.py $G/flint $G/gate-sym-view.jsonl $G/tvr.jsonl,$G/lv54.jsonl,$G/lv18.jsonl $O/gate-codegen.jsonl 44 > $O/gate-progress.log 2>&1
(cd $G && python3 gate_eval.py $O/gate-codegen.jsonl > $O/gate-eval.txt 2>&1)
log "gate DONE | $(grep -c ' OK$' $O/gate-progress.log) OK, $(grep -c 'DIFF' $O/gate-progress.log) DIFF | eval: $(tail -3 $O/gate-eval.txt | tr '\n' ' ')"
echo DONE > $O/DONE; aws s3 cp $O/ s3://flint-tlc-gate-20261005/out/ --recursive --quiet; log DONE
shutdown -h +5
