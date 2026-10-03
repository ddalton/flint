#!/bin/bash
# OPEN3 worlds on TLC, one at a time; expectations are WORLDS.tsv (written
# before any run). Usage: run.sh <jar> <workers> <heap> <tlim s> [world...]
# (default: every world in WORLDS.tsv not yet decided in RESULTS.txt).
set -u
cd "$(dirname "$0")" || exit 1
JAR=$1; W=$2; HEAP=$3; TLIM=$4; shift 4
MD5=$( (md5sum ForgeSyncKeptSet.tla 2>/dev/null || md5 -r ForgeSyncKeptSet.tla) | cut -c1-32)
ST=${ST:-./states}; mkdir -p "$ST" out
echo "TLC $(basename "$JAR"), ForgeSyncKeptSet.tla $MD5, $W workers, -Xmx$HEAP, ${TLIM}s a world; started $(date '+%F %T')" >> RESULTS.txt
worlds=${*:-$(cut -f1 WORLDS.tsv)}
for w in $worlds; do
  grep -qE "^$w +(OK-|MISMATCH)" RESULTS.txt && continue
  exp=$(awk -F'\t' -v w="$w" '$1==w{print $2}' WORLDS.tsv)
  rm -rf "$ST/$w"; t0=$(date +%s)
  java -XX:+UseParallelGC -Xmx$HEAP -cp "$JAR" tlc2.TLC -workers $W -checkpoint 0 \
    -metadir "$ST/$w" -config $w.cfg ForgeSyncKeptSet.tla > out/$w.out 2>&1 &
  pid=$!; why=""
  while kill -0 $pid 2>/dev/null; do
    sleep 5
    [ $(( $(date +%s) - t0 )) -gt "$TLIM" ] && { why=UNDECIDED-TIMEOUT; kill $pid; break; }
  done
  wait $pid; rc=$?
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Temporal properties were violated|No error has been found|^Error: [^.]*" out/$w.out | head -1)
  v=MISMATCH
  if [ -n "$why" ]; then v=$why
  elif [ "$exp" = HOLDS ]; then [ "$got" = "No error has been found" ] && v=OK-HOLDS
  elif echo "$got" | grep -qE "(Invariant|property) $exp is violated"; then v=OK-FIRES; fi
  printf "%-32s %-18s exp=%-42s rc=%-3s | %s | %s | %ss\n" "$w" "$v" "$exp" "$rc" "$got" \
    "$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct states found' out/$w.out | tail -1) $(grep -oE 'depth of the complete state graph search is [0-9]+' out/$w.out | tail -1)" \
    $(( $(date +%s) - t0 )) >> RESULTS.txt
  rm -rf "$ST/$w"
done
