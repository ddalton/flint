#!/bin/bash
# M5 part C, E24: the EDITED LeanP1.tla (a commit's delete recorded in udel):
# NR3, AN3, S3Wide3 + TakeOwed3 in the box worlds; the known-bad runs rebuilt
# from it -- MN2 (gateway delete not in udel) must break NR3, MA1 (gateway
# delete records nothing) must break AN3.  Own directory world-e24.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-e24; cp $D/e24-stage/*.cfg $D/e24-stage/LeanP1.tla $D/e24-stage/MCLeanP1M5R.tla $D/world-e24/; cd $D/world-e24
md5sum LeanP1.tla > $D/out/e24-md5.txt
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
mut() { m=$1; n=$2; mkdir -p $D/mut-e24/$m; cp $D/e24-stage/$m.tla $D/mut-e24/$m/LeanP1.tla; md5sum $D/mut-e24/$m/LeanP1.tla >> $D/out/e24-md5.txt; ( cd $D/mut-e24/$m && cp $D/world-e24/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world-e24/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( mut MN2 E24MN2Rem3-NR3; mut MA1 E24MA1Rem3Age-AN3; run E24Rem3-NR3; run E24Rem3-AN3; run E24Rem3-S3W; run E24Scope-NR3; run E24Scope-AN3; run E24Scope-S3W; run E24Fetch-NR3; run E24Fetch-AN3; run E24Fetch-S3W ) &
( run E24Reader-NR3; run E24Reader-S3W; run E24Rem3Age-NR3; run E24Rem3Age-AN3; run E24Rem3Age-S3W; run E24Reader-AN3 ) & wait
echo E24DONE > $D/out/e24.done
