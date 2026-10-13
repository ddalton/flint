#!/bin/bash
# M5 part C, E19c: RelG2All (RelG plus the tombstone of a committed delete), in the
# small worlds, beside E19; and MN2 in Rem3, the known-bad run.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-c; cp $D/world/*.cfg $D/world/LeanP1.tla $D/world-c/; cp $D/e19c-stage/* $D/world-c/; cd $D/world-c
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; mkdir -p $D/mut-c; cp $D/mut/MN2/LeanP1.tla $D/mut-c/; ( cd $D/mut-c && cp $D/world-c/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-c/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
run E19Fetch-RelG2; mut E19MN2Rem3-RelG2; run E19Scope-RelG2; run E19Rem3-RelG2; run E19Reader-RelG2
echo E19CDONE > $D/out/e19c.done
