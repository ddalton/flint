#!/bin/bash
# M5 part C, E18: S3Wide3 and TakeOwed3 (TakeAt plus the Consume convergence
# case) in E17's five worlds and P3; and the known-bad run, MN2 in Rem3, where
# NR3 fails (E14) so one of the two must.  tla-mc 0.1.2 -compile.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; cd $D/world
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 4h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; ( cd $D/mut/MN2 && cp $D/world/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( run E18Scope; run E18Rem3; run E18Fetch ) & ( mut E18MN2Rem3; run E18Reader; run E18Rem3Age ) & ( run E18P3 ) & wait
echo E18DONE > $D/out/e18.done
