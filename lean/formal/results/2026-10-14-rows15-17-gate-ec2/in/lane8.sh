#!/bin/bash
# Lane 8 (2026-10-14 ~23:50Z).  AckWithheldHolds fired AK1 at depth 25 on
# d3d5290d: a MODEL gap, not a code defect.  A's install surfaced a conflict
# over B's declared version; after the age A's reap took the handle, and
# SurfacedDecl's `c[2] \in live` read that as a loss.  The code copies the
# conflict's bytes to conflicts/ first (preserve_conflict_copy), and the copy
# outlives the reap.  Only a swept upload's conflict names bytes no copy
# keeps: the Writer ghost `lost` records those, and SurfacedDecl accepts a
# conflict whose version is live or not lost (LeanP1.tla 4be149ab; user:
# option A, AckWithheldHolds left to the proof).  Stop lane 7 (DeposeAck on
# the stricter claim: its clean prefix stays its record); re-run the six AK1
# mutants (must fire); then DeposeAck and AckHolds2 until the cutoff.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env
pids() { ps -eo pid,args | awk -v s="$1" 'index($0, s) && !/awk/ {print $1}'; }
P=$(pids "bash /work/lane7.sh"); [ -n "$P" ] && kill -9 $P && echo "LANE8 stopped lane 7's script: $P" >> out/RESULTS.txt
for d in pn7 p2c; do
  P=$(pids "--out /work/out/$d"); [ -n "$P" ] && kill -9 $P && echo "LANE8 stopped the $d gate: $P" >> out/RESULTS.txt
  P=$(pids "/work/out/$d/cache-"); [ -n "$P" ] && kill -9 $P && echo "LANE8 stopped the $d checkers: $P $(date -u +%TZ)" >> out/RESULTS.txt
done
P=$(pids "aws s3 sync"); [ -n "$P" ] && kill $P
sleep 10; rm -rf $W/out/pn7/meta-* $W/out/pn7/cache-* $W/out/pm7/cache-* $W/out/pm7/mut
rm -rf $W/formal5; cp -a $W/formal2 $W/formal5; aws s3 cp s3://$BK/in/formal5-add.tgz /tmp/f5.tgz --quiet && tar xzf /tmp/f5.tgz -C $W/formal5; F=$W/formal5
echo "LANE8 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8); $(free -g | sed -n 2p)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
for m in AckIgnoresWithheld CarrierKeepsDelete AckReadsOwnReport AckIgnoresUncited CarrierPerFold RequestAfterCarry; do
  python3 $F/leanp1-gate.py --only "^LeanP1$m\$" --jobs 1 --workers 192 --cap 600 --compile --out $W/out/pm8 \
    --extra "-fpmem 16000 -queue-mem 60000" >> out/lane8m.log 2>&1
  echo "MUT8 LeanP1$m rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$m " out/lane8m.log | tail -1)" >> out/RESULTS.txt
  rm -rf $W/out/pm8/meta-* $W/out/pm8/cache-*
done
for w in DeposeAck AckHolds2; do
  [ "$(date -u +%H)" = 00 ] && [ "$(date -u +%M)" -ge 30 ] && break
  python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 192 --cap 2400 --compile --out $W/out/pn8 \
    --extra "-fpmem 32000 -queue-mem 60000" >> out/lane8.log 2>&1
  echo "NEW8 LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/lane8.log | tail -1) $(dmesg | grep -c 'Killed process') oom-kills so far" >> out/RESULTS.txt
  rm -rf $W/out/pn8/meta-*
done
echo "LANE8 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
