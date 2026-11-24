#!/bin/bash
# diskguard (10-30 c27): an arm whose queue spills past 88% of the root disk is cut (its compiled checker killed
# by pid); run.sh then records the arm with its last full depth, deletes its metadir and goes on.  Keeps the
# TLAPS job and out/ writable.
while sleep 15; do
  u=$(df --output=pcent / | tail -1 | tr -dc 0-9)
  [ "$u" -ge 88 ] || continue
  for p in $(ps -eo pid,args | grep "[/]work/cache-c[0-9]*/bin/" | awk '{print $1}'); do
    echo "$(date -u +%TZ) disk ${u}%: cut pid $p $(ps -o args= -p $p | grep -oE 'meta-[A-Za-z0-9-]+')" >> /work/out/diskguard.txt
    kill $p
  done
  sleep 90
done
