#!/bin/bash
# Lane 6 (2026-10-14 ~22:05Z).  AckHolds1 fired AK1 at depth 22 on 66e2395f:
# a CLAIM error -- the declared write was published (seq 3), then a UI
# delete removed it (seq 4, in udel), and the ack at seq 4 said ok.  Broken
# now excludes a later acknowledged delete (DeletedLater; LeanP1.tla
# aacd05a4).  Stop lane 5 and the paused mutant; on the corrected model run
# the AK1 mutants first (they must still fire), then the four new HOLDS
# worlds one at a time at 192 workers, then the six large old worlds.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env
pids() { ps -eo pid,args | awk -v s="$1" 'index($0, s) && !/awk/ {print $1}'; }
P=$(pids "bash /work/lane5.sh"); [ -n "$P" ] && kill -9 $P && echo "LANE6 stopped lane 5's script: $P" >> out/RESULTS.txt
for d in pn5 pm4; do
  P=$(pids "--out /work/out/$d"); [ -n "$P" ] && kill -9 $P && echo "LANE6 stopped the $d gate: $P" >> out/RESULTS.txt
  P=$(pids "/work/out/$d/cache-"); [ -n "$P" ] && kill -9 $P && echo "LANE6 stopped the $d checkers: $P $(date -u +%TZ)" >> out/RESULTS.txt
done
P=$(pids "aws s3 sync"); [ -n "$P" ] && kill $P
P=$(ps -eo pid,args | awk '/sleep 60/ && !/awk/ {print $1}'); [ -n "$P" ] && kill $P
sleep 10; rm -rf $W/out/pn5/meta-* $W/out/pm4/meta-* $W/out/pn5/cache-* $W/out/pm4/cache-*
rm -rf $W/formal3; cp -a $W/formal2 $W/formal3; aws s3 cp s3://$BK/in/LeanP1-aacd05a4.tla $W/formal3/LeanP1.tla --quiet; F=$W/formal3
echo "LANE6 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8); $(free -g | sed -n 2p)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
python3 $F/leanp1-gate.py --only '^LeanP1(AckReadsOwnReport|AckIgnoresUncited|CarrierPerFold|RequestAfterCarry|AckIgnoresWithheld)$' \
  --jobs 1 --workers 192 --cap 600 --compile --out $W/out/pm6 --extra "-fpmem 16000 -queue-mem 60000" > out/lane6m.log 2>&1
echo "MUT6 rc=$? $(date -u +%FT%TZ) $(tail -1 out/lane6m.log)" >> out/RESULTS.txt
rm -rf $W/out/pm6/meta-*
for w in AckHolds1 AckWithheldHolds DeposeAck AckHolds2; do
  [ "$(date -u +%H)" = 00 ] && [ "$(date -u +%M)" -ge 30 ] && break
  python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 192 --cap 3000 --compile --out $W/out/pn6 \
    --extra "-fpmem 32000 -queue-mem 60000" >> out/lane6.log 2>&1
  echo "NEW6 LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/lane6.log | tail -1) $(dmesg | grep -c 'Killed process') oom-kills so far" >> out/RESULTS.txt
  rm -rf $W/out/pn6/meta-*
done
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy; do
  [ "$(date -u +%H)" = 00 ] && break
  python3 $W/formal/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 1800 --compile --out $W/out/p2c \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2c.log 2>&1
  echo "PHASE2C $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2c.log | tail -1)" >> out/RESULTS.txt
done
echo "LANE6 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
