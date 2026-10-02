#!/bin/bash
# Every world on tlc-rs, then on TLC, against one module md5. Usage: run.sh <tlc-rs> <tla2tools.jar> <statesdir>
set -u
cd "$(dirname "$0")"
B=$1; JAR=$2; ST=$3; R=RESULTS.txt
echo "ForgeSyncSweepWindow.tla $(md5 -q ForgeSyncSweepWindow.tla); started $(date -u +%FT%TZ)" >> $R
W="ProbeRetrySweepTakesNamed ProbeUnderLeaseSweepDeleted Retry RetryRechecks RetryAtomic RetryUnderLease"
one() { # <checker> <world>
  local c=$1 w=$2 md="$ST/$1-$2" t0 out rc
  rm -rf "${md:?}"; t0=$(date +%s)
  if [ $c = tlcrs ]; then out=$($B -workers 4 -metadir "$md" -config ForgeSyncSweepWindow$w.cfg ForgeSyncSweepWindow.tla 2>&1)
  else out=$(java -XX:+UseParallelGC -Xmx5g -cp "$JAR" tlc2.TLC -workers 4 -checkpoint 0 -metadir "$md" -config ForgeSyncSweepWindow$w.cfg ForgeSyncSweepWindow.tla 2>&1); fi
  rc=$?; rm -rf "${md:?}"
  printf "%-6s %-30s rc=%-3s %5ss | %s\n" $c $w $rc $(( $(date +%s)-t0 )) \
    "$(echo "$out" | grep -E "^Error: |states generated|No error" | grep -v -E "behavior|^Progress" | tr '\n' ' ' | cut -c1-200)" >> $R
}
for w in $W; do one tlcrs $w; done
for w in $W; do one tlc $w; done
echo "DONE $(date -u +%FT%TZ)" >> $R
