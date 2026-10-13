#!/bin/bash
# runner3 (E23): NR4 (NR3 + a commit's delete, `cdel`, as an exemption) in W2C,
# where NR3 fails at d19 (E21); and NR3G, NR3 under the cdel-carrying MCSpec, the
# calibration: it must fail exactly as NR3 did.  Module MCLeanP1M5G (EXTENDS
# MCLeanP1M5R; this MCLeanP1M5R adds E19-E22's definitions, nothing E20 reads).
BK=flint-tlc-m5-20261013; W=/work; mkdir -p $W/e23; cd $W/e23
export HOME=/root; . /root/.cargo/env
aws s3 cp s3://$BK/in3/ $W/e23/ --recursive --exclude 'runner3.sh'; cp $W/LeanP1.tla $W/e23/
echo "RUNNER3 $(date -u +%FT%TZ)" >> $W/out/RESULTS.txt
PIDS=""
for x in "W2C-NR4 80" "W2C-NR3G 48"; do
  set -- $x; n=$1; k=$2
  mkdir -p $W/cache/$n $W/meta/$n
  ( ( time TLAMC_CACHE=$W/cache/$n timeout 10800 tla-mc -compile -workers $k -queue-mem 16000 -fpmem 12000 -checkpoint 0 -metadir $W/meta/$n -config E23$n.cfg MCLeanP1M5G.tla ) > $W/out/E23$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> $W/out/E23$n.out
    echo "E23$n rc=$rc $(grep -h -E 'violated|distinct states found' $W/out/E23$n.out | tr '\n' ' ')" >> $W/out/RESULTS.txt; rm -rf $W/meta/$n ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE3 $(date -u +%FT%TZ)" >> $W/out/RESULTS.txt
