#!/bin/bash
# After runner2's DONE2: the scaling question.  E13P3-AN3 for 120 s each:
# interp at 192 workers, compiled (-compile) at 192 and at 14, each with a
# 15 s perf profile at 60 s.  (2026-10-12, user: "profile ... any bottleneck".)
BK=flint-tlc-m5-20261012; W=/work; cd $W; mkdir -p out/bench
until grep -q '^DONE2' out/RESULTS.txt; do sleep 10; done
while pgrep -f runner2.sh > /dev/null; do sleep 3; done   # its own shutdown +5 is set by then
shutdown -c; shutdown -h +25
one() { name=$1; shift
  ( timeout 120 tla-mc "$@" -config cfg/E13P3-AN3.cfg MCLeanP1M5R.tla > out/bench/$name.out 2>&1 ) &
  sleep 60; P=$(pgrep -x tla-mc | head -1); [ -z "$P" ] && P=$(pgrep -f "MCLeanP1M5R" | head -1)
  top -b -n 2 -d 3 | grep -E "^%Cpu" | tail -1 > out/bench/$name.cpu
  perf record -F 49 -g -p $P -o /tmp/$name.data -- sleep 15 > /dev/null 2>&1
  perf report -i /tmp/$name.data --stdio --no-children --sort symbol --percent-limit 1.5 -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | sed -E "s/ +/ /g" | cut -c1-120 | head -20 > out/bench/$name.perf
  wait
}
one interp192 -workers 192
tla-mc -compile -workers 1 -config cfg/E13P3-AN3.cfg MCLeanP1M5R.tla > out/bench/compile-warmup.out 2>&1 &
sleep 90; pkill -x tla-mc; pkill -f MCLeanP1M5R; sleep 2
one compiled192 -compile -workers 192
one compiled14 -compile -workers 14
echo "BENCHDONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out
shutdown -h +3
