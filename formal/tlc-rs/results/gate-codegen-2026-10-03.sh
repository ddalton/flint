#!/bin/bash
# The codegen-fix gate on the c8g: branch tlc-rs/codegen-scaling-2026-10-03 (1cb096dd).
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; log() { echo "$(date -u +%FT%TZ) GATE $*" | tee -a $O/scale.log; }
B=s3://flint-tlc-scale-20261003/payload
while pgrep -f '[p]go.sh' > /dev/null; do sleep 10; done
log "start (pgo finished)"
G=/opt/gate; rm -rf $G; mkdir -p $G/flint
for f in gate-tree.tgz gate_par.py gate-sweep-2026-09-29.jsonl tvr.jsonl lv54.jsonl lv18.jsonl; do aws s3 cp $B/$f $G/$f --quiet || { log "FETCH-FAILED $f"; exit 1; }; done
tar -xzf $G/gate-tree.tgz -C $G/flint
cd $G/flint/formal/tlc-rs && cargo build --release > $O/gate-build.log 2>&1 || { log BUILD-FAILED; exit 1; }
log "built $(md5sum src/codegen.rs | cut -c1-8) codegen.rs"
# Warm the registry once so the parallel builds do not race on the download.
(cd $G/flint/formal && $G/flint/formal/tlc-rs/target/release/tlc-rs -codegen /data/warm -config FlintReplication.cfg FlintReplication.tla > /dev/null 2>&1 && cd /data/warm && cargo fetch > /dev/null 2>&1); rm -rf /data/warm
rm -rf /data/gate-work
python3 $G/gate_par.py $G/flint $G/gate-sweep-2026-09-29.jsonl $G/tvr.jsonl,$G/lv54.jsonl,$G/lv18.jsonl $O/gate-codegen.jsonl 44 > $O/gate-progress.log 2>&1
log "DONE | $(grep -c ' OK$' $O/gate-progress.log) OK, $(grep -c 'DIFF' $O/gate-progress.log) DIFF, $(grep -c 'cfg gone' $O/gate-progress.log) skipped"
aws s3 cp $O/ s3://flint-tlc-scale-20261003/out/ --recursive --quiet
