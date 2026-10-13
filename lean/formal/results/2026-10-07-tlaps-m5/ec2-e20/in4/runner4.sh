#!/bin/bash
# runner4 (E24): W2C on the EDITED LeanP1.tla (a commit's delete recorded in
# udel, 2026-10-13): NR3 must now hold where it failed at d19 (E21); and
# S3Wide3 + TakeOwed3 (NR3's guarded strengthening) in the two-committer world.
# Starts when E23's NR3G calibration has finished (its cores).
BK=flint-tlc-m5-20261013; W=/work; mkdir -p $W/e24; cd $W/e24
export HOME=/root; . /root/.cargo/env
aws s3 cp s3://$BK/in4/ $W/e24/ --recursive --exclude 'runner4.sh'
until grep -q '^E23W2C-NR3G' $W/out/RESULTS.txt; do sleep 30; done
echo "RUNNER4 $(date -u +%FT%TZ) LeanP1.tla md5 $(md5sum LeanP1.tla | cut -c1-32)" >> $W/out/RESULTS.txt
PIDS=""
for x in "W2C-NR3 24" "W2C-S3W 24"; do
  set -- $x; n=$1; k=$2
  mkdir -p $W/cache/E24$n $W/meta/E24$n
  ( ( time TLAMC_CACHE=$W/cache/E24$n timeout 9000 tla-mc -compile -workers $k -queue-mem 16000 -fpmem 12000 -checkpoint 0 -metadir $W/meta/E24$n -config E24$n.cfg MCLeanP1M5R.tla ) > $W/out/E24$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> $W/out/E24$n.out
    echo "E24$n rc=$rc $(grep -h -E 'violated|distinct states found' $W/out/E24$n.out | tr '\n' ' ')" >> $W/out/RESULTS.txt; rm -rf $W/meta/E24$n ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE4 $(date -u +%FT%TZ)" >> $W/out/RESULTS.txt
