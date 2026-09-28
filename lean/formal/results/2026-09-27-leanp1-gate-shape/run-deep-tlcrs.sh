#!/bin/bash
# LeanP1Holds at its FULL bounds, the opt-in deep run, on tlc-rs (2026-09-28;
# replaces run-deep.sh, which ran TLC and had not started). The build must
# have the disk-backed seen set (551c2ffe or later): past 1B states the
# in-RAM set alone would need 14+ GB. No time limit; a checkpoint every
# 30 min, KEPT. Starts when gate d ends, with the 4 workers gate d frees.
# To resume after a stop or a reboot, run this script again: it recovers
# from the metadir's checkpoint when there is one.
set -u
cd ~/lean-leanp1-deep-2026-09-27 || exit 1
BIN=$1; REV=$2
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED" >> RESULTS-tlcrs.txt; exit 4; }
until grep -q LEANP1DONE ~/lean-leanp1-2026-09-25d/RESULTS.txt 2>/dev/null; do sleep 120; done
ST=/mnt/nvme/tlcrs-leanp1-deep/LeanP1Holds
LIMIT_KB=$((5 * 1024 * 1024))   # 5 GB: fp set 1 GB + queue 1 GB in RAM, the rest spills
if [ -e $ST/ckpt/meta.txt ]; then ARGS="-recover $ST"; how=recover; else rm -rf $ST; mkdir -p $(dirname $ST); ARGS="-metadir $ST"; how=fresh; fi
echo "start $(date -u +%FT%TZ) tlc-rs $REV $how, 4 workers, -fpmem 1024 -queue-mem 1024 -checkpoint 30, RSS guard 5 GB" >> runs.log
nice -n 10 $BIN -workers 4 -fpmem 1024 -queue-mem 1024 -checkpoint 30 $ARGS -config LeanP1Holds.cfg LeanP1.tla >> out-LeanP1Holds-tlcrs.out 2>&1 &
pid=$!; why=""
while kill -0 $pid 2>/dev/null; do
  sleep 60
  rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null || echo 0)
  [ "${rss:-0}" -gt $LIMIT_KB ] && { why="GUARD-RSS ${rss}kB"; kill $pid; break; }
done
wait $pid; rc=$?
if [ -n "$why" ]; then
  echo "LeanP1Holds $why at $(date -u +%FT%TZ) | $(grep '^progress:' out-LeanP1Holds-tlcrs.out | tail -1) (checkpoint kept; rerun to resume)" >> RESULTS-tlcrs.txt
  exit 3
fi
printf "LeanP1Holds rc=%s | %s | %s\n" "$rc" \
  "$(grep -E 'No error has been found|is violated|^Error' out-LeanP1Holds-tlcrs.out | tail -1)" \
  "$(grep -E 'distinct states found' out-LeanP1Holds-tlcrs.out | tail -1)" >> RESULTS-tlcrs.txt
echo DEEPDONE >> RESULTS-tlcrs.txt
