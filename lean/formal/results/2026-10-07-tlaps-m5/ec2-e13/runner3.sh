#!/bin/bash
# runner3 (2026-10-12 ~23:30Z): runner2 went DISK-bound (84% idle, 7% iowait,
# gp3 at its 125 MB/s: every process spilled its BFS queue past the default
# 256 MB).  Now: the COMPILED engine (-compile), the queue in RAM
# (-queue-mem 30000 MB each), 9 runs x 20 workers, 20 min cap.
BK=flint-tlc-m5-20261012; W=/work; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 30; done; }
sync_loop & SYNC=$!
RUNS="P3-NR3 P3Age-NR3 W2Rem3-NR3 P3-AN3 P3Age-AN3 W2Rem3-AN3 P3-Inv_AckedNamed P3Age-Inv_AckedNamed W2Rem3-Inv_AckedNamed"
echo "RUNNER3 $(date -u +%FT%TZ) compiled, 20 workers each, -queue-mem 30000, 20 min cap" >> out/RESULTS.txt
# build once (the cache is shared; parallel builds would race)
timeout 300 tla-mc -compile -workers 1 -config cfg/E13P3-NR3.cfg MCLeanP1M5R.tla > out/compile.out 2>&1 &
CP=$!; sleep 75; kill $CP 2>/dev/null; pkill -f "MCLeanP1M5R" 2>/dev/null; sleep 1
PIDS=""
for n in $RUNS; do
  mv -f out/E13$n.out out/E13$n.par14.out 2>/dev/null
  ( ( time timeout 1200 tla-mc -compile -workers 20 -queue-mem 30000 -config cfg/E13$n.cfg MCLeanP1M5R.tla ) > out/E13$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E13$n.out
    echo "E13$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E13$n.out | tr '\n' ' ')" >> out/RESULTS.txt ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE3 $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
