#!/bin/bash
# runner2 (2026-10-12 23:17Z): the sequential runner at 192 workers spent 60%
# of CPU in Arc refcount atomics (__aarch64_ldadd8_*, from Program::quant
# cloning shared constant sets) -- ~850 distinct/s per core.  Separate
# processes share no refcounts: run all remaining checks AT ONCE, 14 workers
# each, capped at 20 min.  P3-NR2 already decided (violated, depth 13).
BK=flint-tlc-m5-20261012; W=/work; cd $W; mkdir -p out
cp MCLeanP1M5R-nr3.tla MCLeanP1M5R.tla      # NR3 added; earlier ops unchanged
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 30; done; }
sync_loop & SYNC=$!
RUNS="P3-AN3 W2Rem3-NR2 W2Rem3-AN3 P3-Inv_AckedNamed W2Rem3-Inv_AckedNamed P3Age-NR2 P3Age-AN3 P3-Inv_NoRegress W2Rem3-Inv_NoRegress P3Age-Inv_AckedNamed P3Age-Inv_NoRegress P3-NR3 W2Rem3-NR3 P3Age-NR3"
echo "RUNNER2 $(date -u +%FT%TZ) parallel, 14 workers each, 20 min cap" >> out/RESULTS.txt
PIDS=""
for n in $RUNS; do
  mv -f out/E13$n.out out/E13$n.seq.out 2>/dev/null
  ( ( time timeout 1200 tla-mc -workers 14 -config cfg/E13$n.cfg MCLeanP1M5R.tla ) > out/E13$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E13$n.out
    echo "E13$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E13$n.out | tr '\n' ' ')" >> out/RESULTS.txt ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE2 $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
