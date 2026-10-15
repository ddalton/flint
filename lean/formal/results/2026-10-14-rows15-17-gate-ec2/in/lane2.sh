#!/bin/bash
# Lane 2 (sent by SSM, 2026-10-14 ~21:25Z): the worlds with requests, on the
# corrected claims (LeanP1.tla md5 66e2395f: the declaration excludes paths
# whose bytes the document already holds; AK1 no longer requires `partial`
# to name only declared paths).  The old worlds' regression (runner.sh) is
# unaffected: they have no requests, and only ghosts and claims changed.
# Stops runner.sh's new-worlds lane (old claims) first, by PID.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env
for P in $(ps -eo pid,args | awk '/leanp1-gate.py/ && /--out \/work\/out\/pn( |$)/ {print $1}'); do
  L=$(ps -o ppid= -p $P | tr -d ' '); kill -9 $L; pkill -P $P; kill $P; echo "stopped lane N: loop $L, gate $P" >> out/RESULTS.txt
done
mkdir -p $W/formal2 && tar -xzf $W/formal2.tgz -C $W/formal2; F=$W/formal2
echo "LANE2 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
# runner.sh's own end schedules a shutdown in 5 min; while this lane runs,
# replace it with the original hard cap (00:45Z).
( while true; do if grep -q '^DONE' out/RESULTS.txt && [ ! -f $W/lane2.done ]; then shutdown -c; shutdown -h 00:45; sleep 3600; fi; sleep 20; done ) & GUARD=$!
python3 $F/leanp1-gate.py --only '^LeanP1(AckIgnoresWithheld|AckReadsOwnReport|AckIgnoresUncited|CarrierPerFold|RequestAfterCarry|FoldClobbers|StampIndependent|Probe(AckOk|AckPartial|AckDeferred|CapDropped|RetiredAfterRestart))$' \
  --jobs 4 --workers 4 --cap 2400 --compile --out $W/out/pm2 --extra "-fpmem 2000 -queue-mem 8000" > out/lane2m.log 2>&1 &
M=$!
for w in AckCap AckWithheldHolds DeposeAck AckHolds1 AckHolds2; do
  python3 $F/leanp1-gate.py --only "^LeanP1$w\$" --jobs 1 --workers 48 --cap 3000 --compile --out $W/out/pn2 \
    --extra "-fpmem 16000 -queue-mem 48000" >> out/lane2.log 2>&1
  echo "NEW2 LeanP1$w rc=$? $(date -u +%FT%TZ) $(grep "^LeanP1$w " out/lane2.log | tail -1)" >> out/RESULTS.txt
done
wait $M; echo "LANE2M $(date -u +%FT%TZ) $(tail -1 out/lane2m.log)" >> out/RESULTS.txt
echo "LANE2 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt; touch $W/lane2.done
kill $GUARD $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
if grep -q '^DONE' out/RESULTS.txt; then shutdown -c; shutdown -h +5; fi
