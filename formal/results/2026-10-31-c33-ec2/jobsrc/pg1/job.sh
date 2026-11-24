# pg1 (rev 17 61ae6829): G17L/C/Z, the rule's goal check at three replicas (MaxFails 3, the four rule knobs + the
# triple, drain off), NoNewLoss incl. AckedHeld + MembersHold + RaidHolds + AckStamped; bounded 1,800 s, 6 workers each
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/pg1.txt; H=$(pwd)
echo "JOBSTART S $(md5 FlintCatchupS.tla) tla-mc $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ)" > $O
for c in FlintCatchup*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
arm() { c=$1; cap=$2; wk=$3; D=$H/run-$c; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchup$c.cfg $D/; cd $D; t0=$(date +%s)
  timeout $cap env TLAMC_CACHE=$W/cache-pg1-$c $MC -compile -workers $wk -fpmem 2048 -queue-mem 2048 -metadir $W/meta-pg1-$c -config FlintCatchup$c.cfg FlintCatchupS.tla > $OUT/$c.out 2>&1
  rc=$?; rm -rf $W/meta-pg1-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|[Ee]rror:.*" $OUT/$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  echo "ARMDONE $c" >> $O; }
for f in L C Z; do ( arm G17$f 1800 6 ) & done
until [ $(grep -c "^ARMDONE" $O) -ge 3 ]; do sleep 20; done
echo "JOBDONE $(date -u +%TZ)" >> $O
