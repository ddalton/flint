#!/bin/bash
# PGO vs fix #2 on LeanP1Holds, 192 workers, to depth 22, ABAB. Same generated source (/opt/f2/gen).
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; log() { echo "$(date -u +%FT%TZ) PGO $*" | tee -a $O/scale.log; }
M=/opt/payload/model; P2=/data/pgo-prof
rustup component add llvm-tools-preview > $O/pgo-build.log 2>&1 || { log TOOLS-FAILED; exit 1; }
PD=$(ls /root/.rustup/toolchains/*/lib/rustlib/*/bin/llvm-profdata | head -1)
command -v gdb > /dev/null || dnf install -y -q gdb >> $O/pgo-build.log 2>&1 || { log GDB-FAILED; exit 1; }
rm -rf /opt/pgo $P2; mkdir -p /opt/pgo $P2; cp -r /opt/f2/gen /opt/pgo/gen
cd /opt/pgo/gen
RUSTFLAGS="-C target-cpu=native -Cprofile-generate=$P2" CARGO_TARGET_DIR=/opt/pgo/gt-inst cargo build --release >> $O/pgo-build.log 2>&1 || { log INST-BUILD-FAILED; exit 1; }
log "instrumented built"
# Training: depth 17 at 192 workers, then flush the counters through gdb (the checker has no depth limit and a kill skips atexit).
rm -rf /data/tmd
(cd $M && exec /opt/pgo/gt-inst/release/tlcgen-leanp1 -workers 192 -fpmem 32000 -queue-mem 200000 -checkpoint 0 -metadir /data/tmd -config LeanP1Holds.cfg LeanP1.tla > $O/pgo-train.out 2>&1) &
P=$!
while kill -0 $P 2>/dev/null; do sleep 1; grep -q 'depth 17,' $O/pgo-train.out && break; done
gdb -p $P -batch -ex 'call (int)__llvm_profile_write_file()' >> $O/pgo-build.log 2>&1
kill -9 $P; wait $P 2>/dev/null; rm -rf /data/tmd
log "train | $(grep '^progress:' $O/pgo-train.out | grep 'depth 17,' | cut -c1-120) | profraw $(ls $P2 | wc -l) $(du -sh $P2 | cut -f1)"
$PD merge -o /opt/pgo/merged.profdata $P2 >> $O/pgo-build.log 2>&1 || { log MERGE-FAILED; exit 1; }
RUSTFLAGS="-C target-cpu=native -Cprofile-use=/opt/pgo/merged.profdata -Cllvm-args=-pgo-warn-missing-function" CARGO_TARGET_DIR=/opt/pgo/gt-use cargo build --release >> $O/pgo-build.log 2>&1 || { log USE-BUILD-FAILED; exit 1; }
log "pgo built | missing-profile warnings: $(grep -c 'no profile data' $O/pgo-build.log)"
run() { # <label> <binary>
  rm -rf /data/abmd
  (cd $M && exec $2 -workers 192 -fpmem 32000 -queue-mem 200000 -checkpoint 0 -metadir /data/abmd -config LeanP1Holds.cfg LeanP1.tla > $O/ab-$1.out 2>&1) &
  P=$!
  while kill -0 $P 2>/dev/null; do sleep 1; grep -q 'depth 22,' $O/ab-$1.out && { kill $P; break; }; done
  wait $P 2>/dev/null; rm -rf /data/abmd
  log "$1 | $(grep '^progress:' $O/ab-$1.out | grep -E 'depth (18|20|21|22),' | sed -E 's/progress: depth ([0-9]+), ([0-9]+) generated, ([0-9]+) distinct.* ([0-9.]+)s$/d\1 \3 gen=\2 \4s/' | tr '\n' ' ')"
}
run fix2-a /opt/f2/gt/release/tlcgen-leanp1
run pgo-a /opt/pgo/gt-use/release/tlcgen-leanp1
run fix2-b /opt/f2/gt/release/tlcgen-leanp1
run pgo-b /opt/pgo/gt-use/release/tlcgen-leanp1
log DONE
aws s3 cp $O/ s3://flint-tlc-scale-20261003/out/ --recursive --quiet
