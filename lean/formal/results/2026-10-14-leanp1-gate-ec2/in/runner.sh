#!/bin/bash
# The LeanP1 gate on EC2, 2026-10-14 (c8g.48xlarge, 192 vCPU, 384 GB; /work
# on a 1 TB gp3 root): every world of WORLDS-LeanP1.tsv on the restated
# claims (AN5, NR5), compiled tla-mc, and P3Age for AN5 + NR5 on today's
# model.  LeanP1LiveHolds stays out (replaced by LiveHoldsSmall, the user's
# call 2026-09-28).
#   phase 1: every world but the six large, 24 at a time, 4 workers each;
#   P3Age beside it all at 48 workers;
#   phase 2: the six large one at a time at 136 workers, claims first.
# out/ synced to S3 every 60 s.
BK=$1; W=/work; F=$W/formal; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 60; done; }
sync_loop & SYNC=$!
( while true; do { date -u +%TZ; df -h /work | tail -1; free -g | sed -n 2p; } >> out/res.txt; sleep 300; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(tla-mc -version 2>&1 | head -1) $(nproc) cpus; LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8)" > out/RESULTS.txt
BIG='^LeanP1(Holds|Holds1p3b|AllHolds|DeleteOverrideOff|CollectorGreedy|NoConvergence|LiveHolds)$'
( cd $F && mkdir -p $W/meta-p3 $W/cache-p3 && ( time TLAMC_CACHE=$W/cache-p3 timeout 10000 tla-mc -compile -workers 48 \
    -fpmem 24000 -queue-mem 80000 -checkpoint 0 -metadir $W/meta-p3 -config $W/P3Age-AN5-NR5.cfg LeanP1.tla ) > $W/out/P3Age-AN5-NR5.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $W/out/P3Age-AN5-NR5.out; rm -rf $W/meta-p3 $W/cache-p3
  echo "P3Age-AN5-NR5 $(grep -h -o -E 'is violated|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $W/out/P3Age-AN5-NR5.out | tr '\n' ' ')" >> $W/out/RESULTS.txt ) &
P3=$!
python3 $F/leanp1-gate.py --skip "$BIG" --jobs 24 --workers 4 --cap 2400 --compile --out $W/out/p1 \
  --extra "-fpmem 2000 -queue-mem 6000" > out/phase1.log 2>&1
echo "PHASE1 rc=$? $(date -u +%FT%TZ) $(tail -1 out/phase1.log)" >> out/RESULTS.txt
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  python3 $F/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 136 --cap 2400 --compile --out $W/out/p2 \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2.log 2>&1
  echo "PHASE2 $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2.log | tail -1)" >> out/RESULTS.txt
done
wait $P3
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
