#!/bin/bash
# Lane 3 (sent by SSM, 2026-10-14 ~21:45Z; the user: "run the new worlds at
# full throttle"): stop the regression's phase 2 and the VerifyOff re-run,
# and lane 2's one-at-a-time HOLDS loop; run the four remaining new HOLDS
# worlds at once over the whole machine; then, if time is left, phase 2 at
# 192 workers.  Lane 2's last mutant (AckIgnoresWithheld) keeps running.
# The userdata hard cap (shutdown at ~00:48Z) stays armed throughout.
BK=flint-tlc-gate-20261014b; W=/work; cd $W; export HOME=/root; . /root/.cargo/env
stop() { # <regex on the gate's --out dir> <what>
  for P in $(ps -eo pid,args | awk -v re="$1" '/leanp1-gate.py/ && $0 ~ re {print $1}'); do
    L=$(ps -o ppid= -p $P | tr -d ' '); kill -9 $L; pkill -P $P; kill $P
    echo "LANE3 stopped $2: parent $L, gate $P $(date -u +%TZ)" >> out/RESULTS.txt
  done
}
stop '--out /work/out/p2( |$)' 'phase 2 (runner.sh)'
stop '--out /work/out/p1b( |$)' 'the VerifyOff re-run'
stop '--out /work/out/pn2( |$)' "lane 2's HOLDS loop"
sleep 5
F=$W/formal2
echo "LANE3 START $(date -u +%FT%TZ) LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8)" >> out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
python3 $F/leanp1-gate.py --only '^LeanP1(AckWithheldHolds|DeposeAck|AckHolds1|AckHolds2)$' --jobs 4 --workers 46 --cap 7200 --compile \
  --out $W/out/pn3 --extra "-fpmem 16000 -queue-mem 40000" > out/lane3.log 2>&1
echo "LANE3 NEW $(date -u +%FT%TZ) $(tail -1 out/lane3.log)" >> out/RESULTS.txt
for w in LeanP1Holds1p3b LeanP1AllHolds LeanP1Holds LeanP1DeleteOverrideOff LeanP1NoConvergence LeanP1CollectorGreedy LeanP1VerifyOff; do
  python3 $W/formal/leanp1-gate.py --only "^$w\$" --jobs 1 --workers 192 --cap 2400 --compile --out $W/out/p2b \
    --extra "-fpmem 48000 -queue-mem 150000" >> out/phase2b.log 2>&1
  echo "PHASE2B $w rc=$? $(date -u +%FT%TZ) $(grep "^$w " out/phase2b.log | tail -1)" >> out/RESULTS.txt
done
echo "LANE3 DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
