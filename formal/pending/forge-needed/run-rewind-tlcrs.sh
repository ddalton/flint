#!/bin/bash
# The rewind fix's three long worlds on tlc-rs (2026-09-28), one at a time:
# the control that must reproduce the 09-27 counterexample, the same world
# without the proof witness (does the gap LOSE objects?), and Holds with the
# rule. 2 workers, niced; the box is shared (gate d, the shape sizing, Lite).
# Expectations are WORLDS-Rewind.tsv, written before the runs.
# An unfinished run keeps its metadir, to resume with -recover.
set -u
cd "$(dirname "$0")" || exit 1
BIN=$1; REV=$2
MD5=$3   # ForgeSyncRewind.tla as checked in
[ "$(md5sum ForgeSyncRewind.tla | cut -c1-32)" = "$MD5" ] || { echo "MD5 CHANGED ForgeSyncRewind.tla" >> RESULTS-Rewind-fix.txt; exit 4; }
ST=/mnt/nvme2/tlcrs-forge-rewind; mkdir -p $ST out
LIMIT_KB=${LIMIT_KB:-$((6 * 1024 * 1024))}   # 6 GB: the fp set lives in RAM (~14 B/state)
TLIM=${TLIM:-28800}                          # 8 h a world
echo "tlc-rs $REV, ForgeSyncRewind.tla $MD5, 2 workers, checkpoint 15 min, ${TLIM}s a world, RSS guard $((LIMIT_KB/1048576)) GB; started $(date '+%F %T')" >> RESULTS-Rewind-fix.txt
for w in ${WORLDS:-ForgeSyncRewindKeepsNamedRetention ForgeSyncRewindKeepsNamedRetentionLoss ForgeSyncRewindHolds}; do
  exp=$(awk -F'\t' -v w=$w '$1==w{print $2}' WORLDS-Rewind.tsv)
  rm -rf $ST/$w
  t0=$(date +%s)
  nice -n 10 $BIN -workers 2 -checkpoint 15 -metadir $ST/$w -config $w.cfg ForgeSyncRewind.tla > out/$w-tlcrs.out 2>&1 &
  pid=$!; why=""
  while kill -0 $pid 2>/dev/null; do
    sleep 30
    rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null || echo 0)
    [ "${rss:-0}" -gt $LIMIT_KB ] && { why="GUARD-RSS ${rss}kB"; kill $pid; break; }
    [ $(( $(date +%s) - t0 )) -gt $TLIM ] && { why="UNDECIDED-TIMEOUT ${TLIM}s"; kill $pid; break; }
  done
  wait $pid; rc=$?
  secs=$(( $(date +%s) - t0 ))
  if [ -n "$why" ]; then
    printf "%-40s %s after %ss | exp=%s | %s (metadir kept)\n" $w "$why" $secs "$exp" \
      "$(grep '^progress:' out/$w-tlcrs.out | tail -1)" >> RESULTS-Rewind-fix.txt
  else
    got=$(grep -oE 'Invariant [A-Za-z_]+ is violated|No error has been found' out/$w-tlcrs.out | head -1)
    case $exp in
      HOLDS) [ "$got" = "No error has been found" ] && v=OK-HOLDS || v=MISMATCH ;;
      *) [ "$got" = "Invariant $exp is violated" ] && v=OK-FOUND || v=MISMATCH ;;
    esac
    printf "%-40s %-8s exp=%-30s rc=%-3s %ss | %s | %s\n" $w $v "$exp" $rc $secs "$got" \
      "$(grep -E 'distinct states found' out/$w-tlcrs.out | tail -1)" >> RESULTS-Rewind-fix.txt
    rm -rf $ST/$w
  fi
done
echo FORGEREWINDFIXDONE >> RESULTS-Rewind-fix.txt
