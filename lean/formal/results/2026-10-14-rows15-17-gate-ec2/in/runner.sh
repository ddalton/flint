#!/bin/bash
# The LeanP1 gate on EC2, 2026-10-14 evening (c8g.48xlarge, 192 vCPU, 371 GB;
# /work on a 1 TB gp3 root), after rows 15-17 entered LeanP1.tla
# (docs/plans/lean-row15-boundary-verbs-model-plan-2026-10-14.md).
#   THE REGRESSION: every old world must reproduce its count of this
#   afternoon's run (results/2026-10-14-leanp1-gate-ec2/out) and its verdict;
#   the new state is written only when a request or a deposal exists, and
#   every old cfg has MaxTouches = MaxDeposals = 0.
#   THE NEW WORLDS: as WORLDS-LeanP1.tsv expects (written before this run).
# Lanes: P3Age (48 workers) | the new HOLDS worlds one at a time (48 workers,
# 3000 s cap each) | phase 1, every other world but the six large, 20 at a
# time, 4 workers each; then phase 2, the six large one at a time at 144.
# LeanP1LiveHolds stays out (decided by TLAPS L1).
# out/ synced every 60 s WITHOUT tla-mc's metadirs and caches.
BK=$1; W=/work; F=$W/formal; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done; }
sync_loop & SYNC=$!
( while true; do { date -u +%TZ; df -h /work | tail -1; free -g | sed -n 2p; } >> out/res.txt; sleep 300; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(tla-mc -version 2>&1 | head -1) $(nproc) cpus; LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8)" > out/RESULTS.txt
BIG='^LeanP1(Holds|Holds1p3b|AllHolds|DeleteOverrideOff|CollectorGreedy|NoConvergence|LiveHolds)$'
NEW='^LeanP1(AckCap|ResHolds|DeposeNoAgeHolds|AckWithheldHolds|DeposeHolds|DeposeAck|AckHolds1|AckHolds2)$'
( cd $F && mkdir -p $W/meta-p3 $W/cache-p3 && ( time TLAMC_CACHE=$W/cache-p3 timeout 6000 tla-mc -compile -workers 48 \
    -fpmem 24000 -queue-mem 80000 -checkpoint 0 -metadir $W/meta-p3 -config $W/P3Age-AN5-NR5.cfg LeanP1.tla ) > $W/out/P3Age-AN5-NR5.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $W/out/P3Age-AN5-NR5.out; rm -rf $W/meta-p3 $W/cache-p3
  echo "P3Age-AN5-NR5 $(grep -h -o -E 'is violated|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $W/out/P3Age-AN5-NR5.out | tr '\n' ' ')" >> $W/out/RESULTS.txt ) &
P3=$!
( for w in AckCap ResHolds DeposeNoAgeHolds AckWithheldHolds DeposeHolds DeposeAck AckHolds1 AckHolds2; do
    python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 48 --cap 3000 --compile --out $W/out/pn \
      --extra "-fpmem 16000 -queue-mem 48000" >> out/laneN.log 2>&1
    echo "NEW LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/laneN.log | tail -1)" >> out/RESULTS.txt
  done ) &
LN=$!
python3 $F/leanp1-gate.py --skip "$BIG|$NEW" --jobs 20 --workers 4 --cap 2400 --compile --out $W/out/p1 \
  --extra "-fpmem 2000 -queue-mem 6000" > out/phase1.log 2>&1
echo "PHASE1 rc=$? $(date -u +%FT%TZ) $(tail -1 out/phase1.log)" >> out/RESULTS.txt
wait $P3
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  python3 $F/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 144 --cap 2400 --compile --out $W/out/p2 \
    --extra "-fpmem 40000 -queue-mem 120000" >> out/phase2.log 2>&1
  echo "PHASE2 $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2.log | tail -1)" >> out/RESULTS.txt
done
wait $LN
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
