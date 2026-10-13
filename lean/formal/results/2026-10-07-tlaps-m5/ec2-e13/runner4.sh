#!/bin/bash
# runner4: runner3's 7 other compiled runs exited rc=2 -- tla-mc's -compile
# cache is keyed by module name, so 9 parallel builds of one module with
# different cfgs overwrote each other's binary ("generated from different
# sources ... regenerate it").  Each run now gets its own TLAMC_CACHE.
# runner3's P3-NR3 and W2Rem3-NR3 keep running.
BK=flint-tlc-m5-20261012; W=/work; cd $W
export HOME=/root; . /root/.cargo/env
RUNS="P3Age-NR3 P3-AN3 P3Age-AN3 W2Rem3-AN3 P3-Inv_AckedNamed P3Age-Inv_AckedNamed W2Rem3-Inv_AckedNamed"
echo "RUNNER4 $(date -u +%FT%TZ) compiled, own cache each, 20 workers, -queue-mem 30000, 20 min cap" >> out/RESULTS.txt
PIDS=""
for n in $RUNS; do
  mkdir -p /work/cache/$n; cp -r /root/.cache/tla-mc/target /work/cache/$n/ 2>/dev/null
  ( ( time TLAMC_CACHE=/work/cache/$n timeout 1200 tla-mc -compile -workers 20 -queue-mem 30000 -config cfg/E13$n.cfg MCLeanP1M5R.tla ) > out/E13$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E13$n.out
    echo "E13$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E13$n.out | tr '\n' ' ')" >> out/RESULTS.txt ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE4 $(date -u +%FT%TZ)" >> out/RESULTS.txt
