#!/bin/bash
date -u +%TZ; grep progress /work/out/c27-FlintCatchupXZTNJ.out | tail -1
p=$(ps -eo pid,args | grep "[/]work/cache-c27/bin/flintcatchups.*meta-c27-FlintCatchupXZTNJ" | awk '{print $1}')
echo "kill $p"; [ -n "$p" ] && kill $p
for i in $(seq 1 24); do grep -qE "^[A-Z]+ XZTNJ " /work/out/c27.txt && break; sleep 5; done
tail -1 /work/out/c27.txt
echo "CUT XZTNJ $(date -u +%TZ) at the user's word (3-replica zombie space not finishable, disk-bound; little use): bounded, last full depth 27 (2,429,420,385 distinct), no violation" >> /work/out/c27.txt
sleep 30; ps -eo pid,etimes,args | grep "[/]work/cache-c27/bin/\|[t]imeout [0-9]* env" | cut -c1-200; df -h / | tail -1
