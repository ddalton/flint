#!/bin/bash
# LeanP1LiveHolds after the gate (user 2026-10-14): waits for runner.sh to exit,
# cancels its shutdown, runs LiveHolds at 192 workers (4 h cap), syncs, shuts down.
BK=flint-tlc-gate-20261014; W=/work; F=$W/formal
export HOME=/root; . /root/.cargo/env
shutdown -c; shutdown -h +420
while pgrep -f "bash /work/runner.sh" >/dev/null; do sleep 20; done
shutdown -c; shutdown -h +300
echo "LIVE START $(date -u +%FT%TZ)" >> $W/out/RESULTS.txt
( while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 60; done ) & S=$!
python3 $F/leanp1-gate.py --only '^LeanP1LiveHolds$' --jobs 1 --workers 192 --cap 14400 --compile --out $W/out/live \
  --extra "-fpmem 48000 -queue-mem 150000" > $W/out/live.log 2>&1
echo "LIVE rc=$? $(date -u +%FT%TZ) $(tail -2 $W/out/live.log | head -1)" >> $W/out/RESULTS.txt
echo "DONE-LIVE $(date -u +%FT%TZ)" >> $W/out/RESULTS.txt
kill $S; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
