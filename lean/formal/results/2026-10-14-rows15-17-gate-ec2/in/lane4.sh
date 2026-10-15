#!/bin/bash
# Lane 4 (sent by SSM, 2026-10-14 ~21:50Z; the user: run a large world on its
# own).  AckHolds2 (two writers, two paths, x3.3 a level at depth 12) is
# stopped and re-run alone at full width once the three one-path worlds of
# lane 3 finish; the mutant AckIgnoresWithheld goes from 4 workers to 48.
# Lane 3's phase-2 re-run is replaced by this lane's, after AckHolds2.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env; F=$W/formal2
killargs() { # <fixed string in the args> <what>
  local P; P=$(ps -eo pid,args | awk -v s="$1" 'index($0, s) && !/awk/ {print $1}')
  [ -n "$P" ] && kill $P && echo "LANE4 stopped $2: $P $(date -u +%TZ)" >> out/RESULTS.txt
}
killargs "meta-LeanP1AckHolds2" "AckHolds2 (lane 3, 46 workers)"
killargs "/work/out/pm2" "AckIgnoresWithheld (lane 2, 4 workers)"
sleep 5; rm -rf $W/out/pn3/meta-LeanP1AckHolds2 $W/out/pm2/meta-*
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
python3 $F/leanp1-gate.py --only '^LeanP1AckIgnoresWithheld$' --jobs 1 --workers 48 --cap 3600 --compile --out $W/out/pm4 \
  --extra "-fpmem 8000 -queue-mem 30000" > out/lane4m.log 2>&1 &
M=$!
until grep -q '^LANE3 NEW' out/RESULTS.txt; do sleep 20; done
L3=$(ps -eo pid,args | awk 'index($0, "bash /work/lane3.sh") && !/awk/ {print $1}'); [ -n "$L3" ] && kill -9 $L3 && echo "LANE4 stopped lane 3 before its phase 2 ($L3)" >> out/RESULTS.txt
echo "LANE4 ALONE START $(date -u +%FT%TZ)" >> out/RESULTS.txt
WK=192; kill -0 $M 2>/dev/null && WK=144
python3 $F/leanp1-gate.py --only '^LeanP1AckHolds2$' --jobs 1 --workers $WK --cap 6600 --compile --out $W/out/pn4 \
  --extra "-fpmem 48000 -queue-mem 150000" > out/lane4.log 2>&1
echo "LANE4 AckHolds2 $(date -u +%FT%TZ) workers $WK $(grep '^LeanP1AckHolds2 ' out/lane4.log | tail -1)" >> out/RESULTS.txt
wait $M; echo "LANE4M $(date -u +%FT%TZ) $(tail -1 out/lane4m.log)" >> out/RESULTS.txt
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  [ $(date -u +%H%M) -ge 0000 ] && [ $(date -u +%H) = 00 ] && break   # past midnight: no room before the 00:48Z cap
  python3 $W/formal/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 1800 --compile --out $W/out/p2c \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2c.log 2>&1
  echo "PHASE2C $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2c.log | tail -1)" >> out/RESULTS.txt
done
echo "LANE4 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
