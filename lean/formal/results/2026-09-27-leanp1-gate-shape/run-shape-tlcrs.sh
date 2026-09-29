#!/bin/bash
# Gate-shape sizing on tlc-rs (2026-09-28): LeanP1Holds under GateBound at
# three bounds, smallest first, 4 h each. Replaces the TLC runner, which
# waited for gate d; tlc-rs is light enough to run beside it (2 workers,
# niced; the box is shared with the Lite session).
# An unfinished run is recorded two ways: its last `progress:` line, and its
# last checkpoint (fps.bin holds 8 bytes per distinct state).
# A timed-out or guarded run keeps its metadir, to resume with -recover.
set -u
D=~/lean-leanp1-gate-shape-2026-09-27
cd $D || exit 1
BIN=$1   # the tlc-rs binary, built from a named commit
REV=$2   # that commit
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" > RESULTS-tlcrs.txt; exit 4; }
ST=/mnt/nvme2/tlcrs-leanp1-shape; mkdir -p $ST out
LIMIT_KB=${LIMIT_KB:-$((9 * 1024 * 1024))}   # 9 GB: the fp set lives in RAM (~14 B/state)
echo "tlc-rs $REV, 2 workers, checkpoint 15 min, 4 h per world (a resumed world gets 4 h more), RSS guard 9 GB; (re)started $(date '+%F %T')" >> RESULTS-tlcrs.txt
TLIM=${TLIM:-14400}   # overridable only to test the timeout path
for w in ${WORLDS:-MCLeanP1GateSeq3Copies0 MCLeanP1GateSeq3Copies1 MCLeanP1GateSeqAllCopies0}; do
  grep -qE "^$w .*(rc=|UNDECIDED|GUARD)" RESULTS-tlcrs.txt 2>/dev/null && continue   # decided before a pause
  if [ -e $ST/$w/ckpt/meta.txt ]; then ARGS="-recover $ST/$w"; else rm -rf $ST/$w; ARGS="-metadir $ST/$w"; fi
  t0=$(date +%s)
  nice -n 10 $BIN -workers 2 -checkpoint ${CKMIN:-15} $ARGS -config $w.cfg MCLeanP1Gate.tla > out/$w-tlcrs.out 2>&1 &
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
    fps=$(find $ST/$w -path '*/ckpt/fps.bin' 2>/dev/null | head -1)
    meta=$(dirname "$fps" 2>/dev/null)/meta.txt
    n=$( [ -n "$fps" ] && echo $(( $(stat -c %s "$fps") / 8 )) || echo none )
    dep=$( [ -f "$meta" ] && awk '/^depth/{print $2}' "$meta" || echo ? )
    printf "%-28s %s after %ss | last checkpoint: %s distinct, depth %s (metadir kept) | %s\n" $w "$why" $secs "$n" "$dep" \
      "$(grep '^progress:' out/$w-tlcrs.out | tail -1)" >> RESULTS-tlcrs.txt
  else
    printf "%-28s rc=%-3s %ss | %s | %s\n" $w $rc $secs \
      "$(grep -E 'No error has been found|is violated|^Error' out/$w-tlcrs.out | head -1)" \
      "$(grep -E 'distinct states found' out/$w-tlcrs.out | tail -1)" >> RESULTS-tlcrs.txt
    rm -rf $ST/$w
  fi
done
echo SHAPEDONE >> RESULTS-tlcrs.txt
