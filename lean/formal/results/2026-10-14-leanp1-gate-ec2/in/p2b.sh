#!/bin/bash
# Phase 2 restarted at 192 workers (user 2026-10-14 17:29Z: P3Age done, 56 cores
# idle), then LeanP1LiveHolds (192 workers, 4 h cap), then DONE and shutdown.
BK=flint-tlc-gate-20261014; W=/work; F=$W/formal; cd $W
export HOME=/root; . /root/.cargo/env
shutdown -c; shutdown -h +420
( while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 60; done ) & SYNC=$!
( while true; do { date -u +%TZ; df -h /work | tail -1; free -g | sed -n 2p; } >> out/res.txt; sleep 300; done ) & MON=$!
rm -rf $W/out/p2; [ -f out/phase2.log ] && mv out/phase2.log out/phase2-136w-stopped.log
echo "PHASE2 RESTART 192w $(date -u +%FT%TZ)" >> out/RESULTS.txt
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  python3 $F/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 2400 --compile --out $W/out/p2 \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2.log 2>&1
  echo "PHASE2 $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2.log | tail -1)" >> out/RESULTS.txt
done
shutdown -c; shutdown -h +300
echo "LIVE START $(date -u +%FT%TZ)" >> out/RESULTS.txt
python3 $F/leanp1-gate.py --only '^LeanP1LiveHolds$' --jobs 1 --workers 192 --cap 14400 --compile --out $W/out/live \
  --extra "-fpmem 48000 -queue-mem 150000" > out/live.log 2>&1
echo "LIVE rc=$? $(date -u +%FT%TZ) $(grep '^LeanP1LiveHolds ' out/live.log | tail -1)" >> out/RESULTS.txt
echo "DONE-LIVE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
