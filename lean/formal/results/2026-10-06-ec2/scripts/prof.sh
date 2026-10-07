#!/bin/bash
# 30 s perf sample of the running L4W3 checker + CPU/IO split; output to /data/out/prof-<time>.txt
dnf -y install perf sysstat >/dev/null 2>&1
P=$(pgrep tlcgen); T=$(date -u +%H%M); F=/data/out/prof-$T.txt
{ date -u; grep ^progress /data/out/LeanP1Size3L4W3.out | tail -2; tail -1 /data/out/mem.log
  echo "== mpstat 10s (all)"; mpstat 10 1 | tail -2
  echo "== iostat"; iostat -xm 5 2 | awk '/nvme|Device/' | tail -3
  echo "== threads by state"; ps -L -o stat= -p $P | cut -c1 | sort | uniq -c
} > $F 2>&1
cd /data && perf record -F 49 -g -p $P -o /data/perf.data -- sleep 30 >/dev/null 2>&1
{ echo "== perf self (no children)"; perf report -i /data/perf.data --no-children --sort symbol --stdio 2>/dev/null | grep -E '^ +[0-9]' | head -45
  echo "== perf children"; perf report -i /data/perf.data --children --sort symbol --stdio 2>/dev/null | grep -E '^ +[0-9]' | head -40
  echo "== by dso"; perf report -i /data/perf.data --no-children --sort dso --stdio 2>/dev/null | grep -E '^ +[0-9]' | head -10
} >> $F
echo $F
