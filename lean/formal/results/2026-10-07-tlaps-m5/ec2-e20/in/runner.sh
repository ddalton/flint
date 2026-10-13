#!/bin/bash
# M5 E20 on EC2 (c8g.48xlarge, 192 vCPU, 384 GB, 2 TB gp3 at 1000 MB/s at /work):
# NR3 and AN3 in P3Age and W2Rem3 (E13 left them undecided: the 200 GB volume
# filled), and part C's S3X and RelAll in P3, P3Age and W2Rem3.  Ten compiled
# runs at once, each 19 workers, its own cache, -queue-mem 16000 -fpmem 12000,
# no checkpoints, 4 h cap.  out/ synced to S3 every 60 s.
BK=$1; W=/work; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 60; done; }
sync_loop & SYNC=$!
( while true; do { date -u +%TZ; df -h /work | tail -1; free -g | sed -n 2p; } >> out/res.txt; sleep 300; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(tla-mc -version 2>&1 | head -1) $(nproc) cpus" > out/RESULTS.txt
RUNS="P3-S3X P3-Rel P3Age-NR3 P3Age-AN3 P3Age-S3X P3Age-Rel W2Rem3-NR3 W2Rem3-AN3 W2Rem3-S3X W2Rem3-Rel"
PIDS=""
for n in $RUNS; do
  mkdir -p $W/cache/$n $W/meta/$n
  ( ( time TLAMC_CACHE=$W/cache/$n timeout 14400 tla-mc -compile -workers 19 -queue-mem 16000 -fpmem 12000 -checkpoint 0 -metadir $W/meta/$n -config cfg/E20$n.cfg MCLeanP1M5R.tla ) > out/E20$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E20$n.out
    echo "E20$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E20$n.out | tr '\n' ' ')" >> out/RESULTS.txt ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
