#!/bin/bash
# M5 part C, E19b: RelGAll (RelAll guarded to where a take can happen), in the
# small worlds, beside E19; and MN2 in Rem3, the known-bad run.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-b; cp $D/world/*.cfg $D/world/LeanP1.tla $D/world-b/; cp $D/e19b-stage/* $D/world-b/; cd $D/world-b
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; mkdir -p $D/mut-b; cp $D/mut/MN2/LeanP1.tla $D/mut-b/; ( cd $D/mut-b && cp $D/world-b/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-b/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
run E19Scope-RelG; run E19Rem3-RelG; mut E19MN2Rem3-RelG; run E19Fetch-RelG; run E19Reader-RelG
echo E19BDONE > $D/out/e19b.done
