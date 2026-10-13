#!/bin/bash
# M5 E13 on EC2 (c8g.48xlarge, 192 vCPU): the restatement candidates and the
# originals in the three-path, three-path-with-ages and two-writer worlds.
# Each run capped at 10 min; out/ synced to S3 every 30 s (explicit PIDs:
# never a bare `wait` with the sync loop in the background).
BK=$1; W=/work; cd $W; mkdir -p out
sync_loop() { while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 30; done; }
sync_loop & SYNC=$!
ORDER="P3-NR2 P3-AN3 W2Rem3-NR2 W2Rem3-AN3 P3-Inv_AckedNamed W2Rem3-Inv_AckedNamed P3Age-NR2 P3Age-AN3 P3-Inv_NoRegress W2Rem3-Inv_NoRegress P3Age-Inv_AckedNamed P3Age-Inv_NoRegress"
echo "START $(date -u +%FT%TZ) $(tla-mc -version 2>&1 | head -1) $(nproc) cpus" > out/RESULTS.txt
for n in $ORDER; do
  echo "BEGIN E13$n $(date -u +%FT%TZ)" >> out/RESULTS.txt
  ( time timeout 600 tla-mc -workers 192 -config cfg/E13$n.cfg MCLeanP1M5R.tla ) > out/E13$n.out 2>&1
  rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E13$n.out
  echo "E13$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E13$n.out | tr '\n' ' ')" >> out/RESULTS.txt
done
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC; aws s3 sync $W/out s3://$BK/out
shutdown -h +5
