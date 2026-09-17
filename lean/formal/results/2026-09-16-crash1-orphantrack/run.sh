#!/bin/bash
# SAFETY.md §4.4, the half that was never run. One world, one dimension.
#
#   OrphanTrack = FALSE  -> the sweep does not exist. Expect a VIOLATION of
#                           Inv_HITLTracked: the repaired invariant must keep
#                           its teeth in the crash world too.
#   OrphanTrack = TRUE   -> the sweep exists. If this HOLDS, the untracked
#                           sweep is LOAD-BEARING for S1 here, not the churn
#                           control §3 calls it — a change to what it IS.
#
# Both arms run against the SAME spec file, committed at cc43dd8d (the
# Inv_HITLTracked repair). The prior violation on record predates that
# repair, so neither arm's verdict can be carried over.
set -u
L=$(cd "$(dirname "$0")" && pwd); JAR=/Users/ddalton/github/flint/.tla2tools.jar
run() {
  local name=$1 cfg=$2
  [ -f "$L/$name.exit" ] && { echo "$name: already done"; return 0; }
  date -u +%FT%TZ > "$L/$name.start"
  ( cd "$L" && caffeinate -i java -Xmx3g -XX:+UseParallelGC -cp "$JAR" tlc2.TLC \
      -workers 6 -checkpoint 30 -metadir "$L/states/$name" \
      -config "$cfg" LeanSubtree.tla > "$L/$name.log" 2>&1 ) &
  local pid=$!
  while kill -0 $pid 2>/dev/null; do
    free_kb=$(df -k / | tail -1 | awk '{print $4}')
    if [ "$free_kb" -lt 4000000 ]; then
      date -u +%FT%TZ > "$L/$name.DISK-LOW"
      pkill -f "metadir $L/states/$name"
    fi
    sleep 30
  done
  wait $pid; local rc=$?
  echo "$rc $(date -u +%FT%TZ)" > "$L/$name.exit"
  [ -f "$L/$name.DISK-LOW" ] || rm -rf "$L/states/$name"
  echo "$name: rc=$rc"
}
run orphanfalse Crash1OrphanFalse.cfg
run orphantrue  Crash1OrphanTrue.cfg
date -u +%FT%TZ > "$L/ALLDONE"
