#!/bin/bash
date -u +%TZ; free -g | sed -n 2p; df -h /work | tail -1
ps -eo pid,rss,args --sort=-rss | awk '/bin\/leanp1/ && !/awk/ {printf "%s %.1fGB %s\n", $1, $2/1048576, $3}' | cut -c1-110
for f in /work/out/pn*/LeanP1*.out /work/out/pm*/LeanP1*.out; do [ -f $f ] && echo "$f: $(grep -E 'progress|No error|violated|Error' $f | tail -1 | cut -c1-150)"; done
grep -E "LANE|NEW2|PHASE2" /work/out/RESULTS.txt | tail -6 | cut -c1-160
dmesg 2>/dev/null | grep -i "killed process" | tail -3 | cut -c1-150
