#!/bin/bash
# flint-46, 2026-09-29 (user: give the deep run more workers once the others
# finish): when the rewind runner and the long gate sweep are done, restart the
# deep LeanP1Holds from its checkpoint with 8 workers.
cd ~/lean-leanp1-deep-2026-09-27 || exit 1
while pgrep -f 'run-rewind-tlcrs[.]sh' >/dev/null || pgrep -f 'sweep[.]py .*gate-long-v6' >/dev/null; do sleep 120; done
grep -q DEEPDONE RESULTS-tlcrs.txt 2>/dev/null && exit 0
# right after a checkpoint, so the restart loses little
n0=$(grep -c '^checkpoint:' out-LeanP1Holds-tlcrs.out)
until [ "$(grep -c '^checkpoint:' out-LeanP1Holds-tlcrs.out)" -gt "$n0" ]; do sleep 20; done
sleep 10
# the runner first: once its checker exits it records a result and DEEPDONE
pkill -f 'run-deep-tlcrs[.]sh'
sleep 3
pkill -f 'tlc-rs-551c2ffe/.*LeanP1Holds.cfg'
sleep 5
[ -e /mnt/nvme/tlcrs-leanp1-deep/LeanP1Holds/ckpt/meta.txt ] || { echo "SCALE ABORTED $(date -u +%FT%TZ): no checkpoint" >> runs.log; exit 3; }
echo "RESCALED $(date -u +%FT%TZ) (flint-46, user's call): stopped after checkpoint $(grep '^checkpoint:' out-LeanP1Holds-tlcrs.out | tail -1 | grep -oE 'depth [0-9]+, [0-9]+ distinct'); resuming with 8 workers" >> runs.log
WORKERS=8 RSS_GB=8 exec ./run-deep-tlcrs-2.sh /home/ddalton/tlc-rs-551c2ffe/formal/tlc-rs/target/release/tlc-rs 551c2ffe
