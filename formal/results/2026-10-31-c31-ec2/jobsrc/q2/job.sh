# q2 on c31 (tla-mc 0.2.2; rev 17 61ae6829): G17DL/C/Z (the goal check at three replicas, the drain in), Z4Ack17 (the
# zombie triple at four replicas, the clean rerun), X4D17 (X4ZTNJ at MaxFails 3 on rev 17, the drain in); bounded 1,800 s
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/q2.txt; H=$(pwd)
echo "JOBSTART S $(md5 FlintCatchupS.tla) tla-mc $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ)" > $O
for c in FlintCatchup*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
arm() { c=$1; cap=$2; wk=$3; D=$H/run-$c; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchup$c.cfg $D/; cd $D; t0=$(date +%s)
  timeout $cap env TLAMC_CACHE=$W/cache-q2-$c $MC -compile -workers $wk -fpmem 16384 -queue-mem 16384 -metadir $W/meta-q2-$c -config FlintCatchup$c.cfg FlintCatchupS.tla > $OUT/$c.out 2>&1
  rc=$?; rm -rf $W/meta-q2-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|[Ee]rror:.*" $OUT/$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  echo "ARMDONE $c" >> $O; }
for f in L C Z; do ( arm G17D$f 1800 12 ) & done
( arm Z4Ack17 1800 16 ) &
( arm X4D17 1800 16 ) &
until [ $(grep -c "^ARMDONE" $O) -ge 5 ]; do sleep 20; done
echo "JOBDONE $(date -u +%TZ)" >> $O
