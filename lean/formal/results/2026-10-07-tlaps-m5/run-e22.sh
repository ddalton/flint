#!/bin/bash
# M5 part C, E22: NoOlder (S3Wide3's rename case: no strictly older version of an
# unexempted take-able baseline is published while its path is empty), in the box
# worlds, and MN2 in Rem3 (the known-bad run).  Own directory: world-e22.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-e22; cp $D/world/LeanP1.tla $D/e22-stage/* $D/world-e22/; cd $D/world-e22
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; mkdir -p $D/mut-e22; cp $D/mut/MN2/LeanP1.tla $D/mut-e22/; ( cd $D/mut-e22 && cp $D/world-e22/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-e22/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( run E22Fetch; run E22Scope; run E22Rem3 ) & ( mut E22MN2Rem3; run E22Reader ) & ( run E22Rem3Age ) & wait
echo E22DONE > $D/out/e22.done
