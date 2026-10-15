#!/bin/bash
# Lane 7 (2026-10-14 ~22:25Z).  Lane 6's claim fix (DeletedLater) was WRONG:
# the control AckReadsOwnReport stopped firing (its depth-15 trace is a UI
# delete after the carrying CAS, which H10 says the ack must name).  The
# real cause of AckHolds1's d22 trace is a CODE defect, L-136: the carrier
# keeps a path in `deletes` after a later carry uploads it, so a peer's
# delete of the upload is not named.  LeanP1.tla d3d5290d: DeletedLater
# reverted, Install's carrier `deletes = (@ \ uploads) \cup deletes`;
# mutant CarrierKeepsDelete (the old line) must fire AK1.
# Stop lane 6; run the six AK1 mutants (must fire), then the four new HOLDS
# worlds one at a time at 192 workers, then the six large old worlds.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env
pids() { ps -eo pid,args | awk -v s="$1" 'index($0, s) && !/awk/ {print $1}'; }
P=$(pids "bash /work/lane6.sh"); [ -n "$P" ] && kill -9 $P && echo "LANE7 stopped lane 6's script: $P" >> out/RESULTS.txt
for d in pm6 pn6 p2c; do
  P=$(pids "--out /work/out/$d"); [ -n "$P" ] && kill -9 $P && echo "LANE7 stopped the $d gate: $P" >> out/RESULTS.txt
  P=$(pids "/work/out/$d/cache-"); [ -n "$P" ] && kill -9 $P && echo "LANE7 stopped the $d checkers: $P $(date -u +%TZ)" >> out/RESULTS.txt
  P=$(pids "/work/out/$d/mut/"); [ -n "$P" ] && kill -9 $P && echo "LANE7 stopped the $d mutant checkers: $P" >> out/RESULTS.txt
done
P=$(pids "aws s3 sync"); [ -n "$P" ] && kill $P
sleep 10; rm -rf $W/out/pm6/meta-* $W/out/pn6/meta-* $W/out/pm6/cache-* $W/out/pn6/cache-* $W/out/pm6/mut
rm -rf $W/formal4; cp -a $W/formal2 $W/formal4; aws s3 cp s3://$BK/in/formal4-add.tgz /tmp/f4.tgz --quiet && tar xzf /tmp/f4.tgz -C $W/formal4; F=$W/formal4
echo "LANE7 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8); $(free -g | sed -n 2p)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
for m in CarrierKeepsDelete AckReadsOwnReport AckIgnoresUncited CarrierPerFold RequestAfterCarry AckIgnoresWithheld; do
  python3 $F/leanp1-gate.py --only "^LeanP1$m\$" --jobs 1 --workers 192 --cap 600 --compile --out $W/out/pm7 \
    --extra "-fpmem 16000 -queue-mem 60000" >> out/lane7m.log 2>&1
  echo "MUT7 LeanP1$m rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$m " out/lane7m.log | tail -1)" >> out/RESULTS.txt
  rm -rf $W/out/pm7/meta-*
done
for w in AckHolds1 AckWithheldHolds DeposeAck AckHolds2; do
  [ "$(date -u +%H)" = 00 ] && [ "$(date -u +%M)" -ge 30 ] && break
  python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 192 --cap 3000 --compile --out $W/out/pn7 \
    --extra "-fpmem 32000 -queue-mem 60000" >> out/lane7.log 2>&1
  echo "NEW7 LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/lane7.log | tail -1) $(dmesg | grep -c 'Killed process') oom-kills so far" >> out/RESULTS.txt
  rm -rf $W/out/pn7/meta-*
done
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  [ "$(date -u +%H)" = 00 ] && break
  python3 $W/formal/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 1800 --compile --out $W/out/p2c \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2c.log 2>&1
  echo "PHASE2C $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2c.log | tail -1)" >> out/RESULTS.txt
done
echo "LANE7 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
