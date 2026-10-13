#!/bin/bash
# M5 part C, E19: S3X (BackR3 only in the cased-delete excuse) and RelAll (the
# relation S3X needs to be inductive), each alone, in the small E18 worlds; and
# the known-bad runs, MN2 in Rem3 (NR3 fails there, E14).  One lane alongside
# E18, a second once E18 is done.  P3 goes to EC2 with P3Age and W2Rem3.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; cd $D/world
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 4h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { n=$1; ( cd $D/mut/MN2 && cp $D/world/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( mut E19MN2Rem3-Rel; mut E19MN2Rem3-S3X; run E19Rem3-Rel; run E19Scope-Rel; run E19Rem3-S3X; run E19Scope-S3X; run E19Fetch-Rel; run E19Fetch-S3X ) &
( until [ -f $D/out/e18.done ]; do sleep 60; done; run E19Reader-Rel; run E19Rem3Age-Rel; run E19Reader-S3X; run E19Rem3Age-S3X ) & wait
echo E19DONE > $D/out/e19.done
