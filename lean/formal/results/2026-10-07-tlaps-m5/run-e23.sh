#!/bin/bash
# M5 part C, E23: NR4 (NR3 + cdel, a commit's delete, as an exemption; module
# MCLeanP1M5G) in the box worlds, and MN2 in Rem3 -- the known-bad run: cdel
# covers commits only, so the gateway's unrecorded delete must still break NR4.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-e23; cp $D/world/LeanP1.tla $D/e23-stage/* $D/world-e23/; cd $D/world-e23
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5G.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; mkdir -p $D/mut-e23; cp $D/mut/MN2/LeanP1.tla $D/mut-e23/; ( cd $D/mut-e23 && cp $D/world-e23/MCLeanP1M5G.tla $D/world-e23/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-e23/$n.cfg MCLeanP1M5G.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut E23MN2Rem3-NR4; run E23Rem3-NR4; run E23Scope-NR4; run E23Fetch-NR4; run E23Reader-NR4; run E23Rem3Age-NR4
echo E23DONE > $D/out/e23.done
