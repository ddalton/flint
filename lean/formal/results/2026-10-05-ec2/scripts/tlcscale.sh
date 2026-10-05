#!/bin/bash
# #2, conditional (user: "only if it scales", and only with time left): TLC on LeanP1AllHolds.
# 10 min at 48 workers, 10 min at 192; continue to a full run only if 192 is >= 2.5x 48's rate
# AND the projected 756M distinct fit before the deadline (minus 30 min).
set -u; DL=$1; O=/data/out; F=/opt/flint/lean/formal; J=/data/tla2tools.jar
left=$(( DL - $(date +%s) )); [ $left -lt 3600 ] && { echo "TLC: skipped, ${left}s left" >> $O/RESULTS.txt; exit 0; }
dnf -y install java-21-amazon-corretto-headless >/dev/null 2>&1; curl -sSfL -o $J https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar || { echo "TLC: jar fetch failed" >> $O/RESULTS.txt; exit 0; }
probe() { rm -rf /data/st/tlc$1; ( cd $F && timeout 600 java -XX:+UseParallelGC -Xmx300g -cp $J tlc2.TLC -workers $1 -checkpoint 0 -metadir /data/st/tlc$1 -config LeanP1AllHolds.cfg MCLeanP1All.tla > $O/TLC-AllHolds-w$1-probe.out 2>&1 ); rm -rf /data/st/tlc$1
  grep -oE "[0-9,]+ distinct states found" $O/TLC-AllHolds-w$1-probe.out | tail -1 | tr -d , | awk '{print $1}'; }
d48=$(probe 48); d192=$(probe 192)
echo "TLC scaling on AllHolds, 600 s each: 48 workers ${d48:-?} distinct, 192 workers ${d192:-?}" >> $O/RESULTS.txt
[ -n "$d48" ] && [ -n "$d192" ] && [ "$d48" -gt 0 ] || { echo "TLC: no rate, stopping" >> $O/RESULTS.txt; exit 0; }
ratio=$(( d192 * 100 / d48 )); need=$(( 756000000 * 600 / (d192 > 0 ? d192 : 1) )); left=$(( DL - $(date +%s) - 1800 ))
echo "TLC: 192/48 = ${ratio}%, a full run needs ~${need}s at 192, ${left}s usable" >> $O/RESULTS.txt
if [ $ratio -ge 250 ] && [ $need -lt $left ]; then
  rm -rf /data/st/tlcfull; ( cd $F && timeout $left java -XX:+UseParallelGC -Xmx300g -cp $J tlc2.TLC -workers 192 -checkpoint 0 -metadir /data/st/tlcfull -config LeanP1AllHolds.cfg MCLeanP1All.tla > $O/TLC-AllHolds.out 2>&1 ); rc=$?
  echo "TLC AllHolds full: rc=$rc | $(grep -oE 'No error has been found|Invariant [A-Za-z_]+ is violated' $O/TLC-AllHolds.out | head -1) | $(grep -oE '[0-9,]+ states generated, [0-9,]+ distinct states found' $O/TLC-AllHolds.out | tail -1)" >> $O/RESULTS.txt
else echo "TLC: does not scale or does not fit — not run (user's condition)" >> $O/RESULTS.txt; fi
