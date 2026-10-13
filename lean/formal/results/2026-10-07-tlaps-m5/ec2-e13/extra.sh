#!/bin/bash
# After runner.sh's DONE: cancel its shutdown, run NR3 in the three worlds, then
# sync and shut down.  (Started over SSM, 2026-10-12.)
BK=flint-tlc-m5-20261012; W=/work; cd $W
aws s3 cp s3://$BK/extra/ $W/ --recursive
until grep -q '^DONE' out/RESULTS.txt; do sleep 10; done
shutdown -c; shutdown -h +60
cp MCLeanP1M5R-nr3.tla MCLeanP1M5R.tla
for n in P3-NR3 W2Rem3-NR3 P3Age-NR3; do
  echo "BEGIN E13$n $(date -u +%FT%TZ)" >> out/RESULTS.txt
  ( time timeout 600 tla-mc -workers 192 -config cfg/E13$n.cfg MCLeanP1M5R.tla ) > out/E13$n.out 2>&1
  rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E13$n.out
  echo "E13$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E13$n.out | tr '\n' ' ')" >> out/RESULTS.txt
done
echo "EXTRADONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out
shutdown -h +5
