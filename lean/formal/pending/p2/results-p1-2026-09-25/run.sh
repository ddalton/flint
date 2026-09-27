#!/bin/bash
# Paired P1-lite / P2R worlds, locally.  Usage: run.sh <world...>
cd /Users/ddalton/github/flint/lean/formal/pending/p2 || exit 1
W=/private/tmp/claude-503/-Users-ddalton-github-flint/04b1b4fc-8045-4244-87a1-599b98585b0c/scratchpad/p1c
md5 -q LeanCoreP1.tla LeanCoreP2R.tla > $W/md5s.txt
for c in "$@"; do
  m=LeanCoreP1; case $c in P2R*) m=LeanCoreP2R;; esac
  exp=$(awk -F'\t' -v w=$c '$1==w{print $2}' WORLDS-P1.tsv)
  rm -rf $W/$c
  timeout 10800 java -XX:+UseParallelGC -Xmx4g -cp ../../../../.tla2tools.jar tlc2.TLC -workers 4 \
    -metadir $W/$c -config $c.cfg $m.tla > $W/$c.out 2>&1
  rc=$?
  got=$(grep -oE 'Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|No error has been found|Error: [^.]*' $W/$c.out | head -1)
  v=MISMATCH
  if [ $rc = 124 ]; then v=TIMEOUT
  elif [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ]; then v=OK-HOLDS
  elif [ "$exp" = "?" ]; then v=RECORDED
  elif [ "$exp" != HOLDS ] && echo "$got" | grep -q "$exp"; then v=OK-FIRES; fi
  printf "%-26s %-9s rc=%-3s %s | %s | depth %s | %s\n" $c $v $rc "$got" \
    "$(grep -oE '[0-9,]+ distinct states found' $W/$c.out | tail -1)" \
    "$(grep -oE 'depth of the complete state graph search is [0-9]+' $W/$c.out | tail -1 | grep -oE '[0-9]+$')" \
    "$(grep -cE '^State [0-9]+:' $W/$c.out) steps" >> $W/RESULTS.txt
  rm -rf $W/$c   # the state directory: the verdict and the trace are in $c.out
done
echo "BATCHDONE $*" >> $W/RESULTS.txt
