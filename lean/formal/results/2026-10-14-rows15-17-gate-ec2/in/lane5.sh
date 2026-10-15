#!/bin/bash
# Lane 5 (2026-10-14 ~21:55Z; the user: "run them one by one").  MEMORY is
# the limit: the new worlds' states are larger (the Writer record's new
# fields), ~300 B of RSS a state beyond the fingerprint and queue caps;
# AckHolds2 was OOM-killed at 115M states (99 GB) beside the others.  Stop
# lane 3's three parallel worlds and lane 4's loop; run the four new HOLDS
# worlds ONE AT A TIME at 192 workers; then the six large old worlds.  The
# mutant AckIgnoresWithheld (48 workers, small) keeps running.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env; F=$W/formal2
pids() { ps -eo pid,args | awk -v s="$1" 'index($0, s) && !/awk/ {print $1}'; }
P=$(pids "bash /work/lane4.sh"); [ -n "$P" ] && kill -9 $P && echo "LANE5 stopped lane 4's main loop: $P" >> out/RESULTS.txt
P=$(pids "bash /work/lane3.sh"); [ -n "$P" ] && kill -9 $P && echo "LANE5 stopped lane 3's script: $P" >> out/RESULTS.txt
P=$(pids "--out /work/out/pn3"); [ -n "$P" ] && kill $P && echo "LANE5 stopped lane 3's gate: $P" >> out/RESULTS.txt
P=$(pids "/work/out/pn3/cache-"); [ -n "$P" ] && kill $P && echo "LANE5 stopped lane 3's checkers: $P $(date -u +%TZ)" >> out/RESULTS.txt
sleep 10; rm -rf $W/out/pn3/meta-* $W/out/pn2/meta-* $W/out/pn/meta-* $W/out/pn4/meta-*
echo "LANE5 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
for w in AckHolds1 AckWithheldHolds DeposeAck AckHolds2; do
  [ "$(date -u +%H)" = 00 ] && [ "$(date -u +%M)" -ge 30 ] && break
  python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 192 --cap 3000 --compile --out $W/out/pn5 \
    --extra "-fpmem 32000 -queue-mem 60000" >> out/lane5.log 2>&1
  echo "NEW5 LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/lane5.log | tail -1) $(dmesg | grep -c 'Killed process') oom-kills so far" >> out/RESULTS.txt
  rm -rf $W/out/pn5/meta-*
done
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  [ "$(date -u +%H)" = 00 ] && break
  python3 $W/formal/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 1800 --compile --out $W/out/p2c \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2c.log 2>&1
  echo "PHASE2C $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2c.log | tail -1)" >> out/RESULTS.txt
done
echo "LANE5 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
