#!/bin/bash
# runner2 (E21): NR3 in W2C -- two COMMITTING writers with gateway removals,
# a combination no earlier world has (FetchHolds: no removals; W2Rem3: B is a
# reader).  The suspected counterexample: A's committed delete leaves only a
# tombstone, the gateway renames an older version back, B steps back.
# Started beside runner.sh on the cores its finished runs freed.
BK=flint-tlc-m5-20261013; W=/work; cd $W
export HOME=/root; . /root/.cargo/env
aws s3 cp s3://$BK/in2/ $W/cfg/ --recursive --exclude 'runner2.sh'
echo "RUNNER2 $(date -u +%FT%TZ)" >> out/RESULTS.txt
PIDS=""
for n in W2C-NR3 W2CR3-NR3; do
  mkdir -p $W/cache/$n $W/meta/$n
  ( ( time TLAMC_CACHE=$W/cache/$n timeout 10800 tla-mc -compile -workers 32 -queue-mem 16000 -fpmem 12000 -checkpoint 0 -metadir $W/meta/$n -config cfg/E21$n.cfg MCLeanP1M5R.tla ) > out/E21$n.out 2>&1
    rc=$?; echo "rc=$rc $(date -u +%H:%M:%SZ)" >> out/E21$n.out
    echo "E21$n rc=$rc $(grep -h -E 'violated|distinct states found' out/E21$n.out | tr '\n' ' ')" >> out/RESULTS.txt ) &
  PIDS="$PIDS $!"
done
wait $PIDS
echo "DONE2 $(date -u +%FT%TZ)" >> out/RESULTS.txt
