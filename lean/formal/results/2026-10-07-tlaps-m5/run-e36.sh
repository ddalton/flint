#!/bin/bash
# M5 part C, E36: the planned M5C strengthening (MCLeanP1M5C.tla, J5C) as state
# invariants on the final model; MN2 is the known-bad run (PC1 must break).
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e36; cd $W
run() { n=$1; dir=$W; case $n in *MN2*) dir=$D/mut-e36; mkdir -p $dir; cp $D/e35-stage/mut/MN2/LeanP1.tla $W/MCLeanP1M5R.tla $W/MCLeanP1M5C.tla $dir/;; esac
  ( cd $dir && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M5C.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $dir/meta; }
for n in E36MN2Rem3-J5C E36Rem3-J5C E36Scope-J5C E36Reader-J5C E36Rem3Age-J5C; do run $n; done
echo E36DONE > $D/out/e36.done
