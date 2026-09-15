#!/bin/bash
# Local runs of the aborted box's one-path worlds, one at a time, on an 8 GB Mac.
# caffeinate keeps the Mac awake; -checkpoint 30 lets a stopped run resume (-recover);
# a run is stopped (DISK-LOW) before free disk falls under 3 GB. A run with .exit is skipped.
set -u
L=$(cd "$(dirname "$0")" && pwd); JAR=/Users/ddalton/github/flint/.tla2tools.jar
run() { # name cfg
  local name=$1 cfg=$2
  [ -f "$L/$name.exit" ] && return 0
  date -u +%FT%TZ > "$L/$name.start"
  ( cd "$L" && caffeinate -i java -Xmx3g -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 6 -checkpoint 30 \
      -metadir "$L/states/$name" -config "$cfg" LeanSubtree.tla > "$L/$name.log" 2>&1 ) &
  local pid=$!
  while kill -0 $pid 2>/dev/null; do
    free_kb=$(df -k / | tail -1 | awk '{print $4}')
    if [ "$free_kb" -lt 3000000 ]; then date -u +%FT%TZ > "$L/$name.DISK-LOW"; pkill -f "metadir $L/states/$name"; fi
    sleep 30
  done
  wait $pid; local rc=$?
  echo "$rc $(date -u +%FT%TZ)" > "$L/$name.exit"
  [ -f "$L/$name.DISK-LOW" ] || rm -rf "$L/states/$name"
}
run impl1 LeanBarrierLeaseSentinelImpl1.cfg
run fastpath-unguarded LeanBarrierLeaseImplFastPathUnguarded.cfg
run crash1 LeanBarrierLeaseSentinelImplCrash1.cfg
date -u +%FT%TZ > "$L/ALLDONE"
