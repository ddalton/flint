#!/bin/bash
# M5 part C, E25: NoOlder on the EDITED LeanP1.tla.  E22 failed it in Rem3,
# ScopeHolds, Rem3Age (d16) only where the tree's own commit had installed the
# delete -- recorded in udel now.  MN2 (rebuilt, E24) is the known-bad run.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-e25; cp $D/e25-stage/*.cfg $D/world-e25/; cp $D/e24-stage/LeanP1.tla $D/e24-stage/MCLeanP1M5R.tla $D/world-e25/; cd $D/world-e25
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; mkdir -p $D/mut-e25; cp $D/e24-stage/MN2.tla $D/mut-e25/LeanP1.tla; ( cd $D/mut-e25 && cp $D/world-e25/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-e25/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
run E25Rem3-NoOlder; run E25Scope-NoOlder; mut E25MN2Rem3-NoOlder; run E25Fetch-NoOlder; run E25Rem3Age-NoOlder; run E25Reader-NoOlder
echo E25DONE > $D/out/e25.done
