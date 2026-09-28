#!/bin/bash
# The liveness claim at a bound that can finish (2026-09-28, the user's call):
# LiveHoldsSmall and its control LiveWaitsOnLeaseSmall (one path, no
# removals; 10,676,334 distinct, depth 36). Each on tlc-rs (fast; liveness
# graph in memory) and then on TLC (the gate's reference, -lncheck final for
# the HOLDS world). The control first, each time: a HOLDS is only read once
# the same bound has been shown to fail without the rule.
# After both, waits for SMALLDONE and writes LEANP1DONE: gate d is done.
set -u
cd ~/lean-leanp1-2026-09-25d || exit 1
BIN=$1; REV=$2
[ "$(md5sum LeanP1.tla | cut -c1-32)" = 3f6af642f7e39c19cafa413540e50cbe ] || { echo "MD5 CHANGED LeanP1.tla" >> RESULTS.txt; exit 4; }
JAR=~/lean-gate-2026-09-18/.tla2tools.jar
ST=/mnt/nvme2/leanp1-live-small; mkdir -p $ST out
LIMIT_KB=$((6 * 1024 * 1024))
verdict() { # <world> <exp> <got>
  if [ "$2" = HOLDS ] && [ "$3" = "No error has been found" ]; then echo OK-HOLDS
  elif [ "$2" != HOLDS ] && echo "$3" | grep -q "$2"; then echo OK-FIRES
  else echo MISMATCH; fi
}
for chk in tlcrs tlc; do
  for wld in LeanP1LiveWaitsOnLeaseSmall LeanP1LiveHoldsSmall; do
    exp=$(awk -F'\t' -v w=$wld '$1==w{print $2}' WORLDS-LeanP1.tsv)
    rm -rf $ST/$wld-$chk; t0=$(date +%s); why=""
    if [ $chk = tlcrs ]; then
      nice -n 10 $BIN -workers 2 -metadir $ST/$wld-$chk -config $wld.cfg LeanP1.tla > out/$wld-tlcrs.out 2>&1 &
      pid=$!
      while kill -0 $pid 2>/dev/null; do
        sleep 10
        rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null || echo 0)
        [ "${rss:-0}" -gt $LIMIT_KB ] && { why=GUARD-RSS; kill $pid; break; }
      done
      wait $pid; rc=$?
      tag="tlc-rs $REV"
      cnt=$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' out/$wld-tlcrs.out | tail -1)
    else
      LN=; [ "$exp" = HOLDS ] && LN="-lncheck final"
      nice -n 10 java -XX:+UseParallelGC -Xmx8g -cp $JAR tlc2.TLC -workers 2 $LN -checkpoint 0 \
        -metadir $ST/$wld-$chk -config $wld.cfg LeanP1.tla > out/$wld-tlc.out 2>&1
      rc=$?
      tag="TLC${LN:+ $LN}"
      cnt="$(grep -oE '^[0-9,]+ states generated, [0-9,]+ distinct' out/$wld-tlc.out | tail -1) $(grep -oE 'depth of the complete state graph search is [0-9]+' out/$wld-tlc.out | tail -1)"
    fi
    got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Temporal propert[a-z]* [^.]*violated|No error has been found|Error: [^.]*" out/$wld-$chk.out | head -1)
    v=${why:-$(verdict $wld "$exp" "$got")}
    printf "%-30s %-18s exp=%-22s rc=%-3s | %s | %s | %ss [%s]\n" "$wld" "$v" "$exp" "$rc" "$got" "$cnt" $(( $(date +%s) - t0 )) "$tag" >> RESULTS.txt
    rm -rf $ST/$wld-$chk
  done
done
until grep -q SMALLDONE RESULTS.txt 2>/dev/null; do sleep 60; done
echo LEANP1DONE >> RESULTS.txt
