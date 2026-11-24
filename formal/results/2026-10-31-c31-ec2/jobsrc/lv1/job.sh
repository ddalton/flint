# lv1 on c31 (tla-mc 0.2.2): liveness on rev 17 Full, two replicas: L17L/C/Z (Prop_BarrierEnds, Spec), their RecordUp
# FALSE controls (expect lassos), the SpecNoReturn arms (as c30), and c30b's positive control pair; through the wrapper
# FlintCatchupSLiveW.tla (752b1a56: FairnessNoReturn = Fairness less WF Return; Fairness is unchanged rev 12 -> 17)
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/lv1.txt; H=$(pwd)
echo "JOBSTART S $(md5 FlintCatchupS.tla) wrap $(md5 FlintCatchupSLiveW.tla) $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ)" > $O
for c in FlintCatchup*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
arm() { c=$1; cap=$2; wk=$3; D=$H/run-$c; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchupSLiveW.tla $H/FlintCatchup$c.cfg $D/; cd $D; t0=$(date +%s)
  timeout $cap env TLAMC_CACHE=$W/cache-lv1-$c $MC -compile -workers $wk -fpmem 16384 -queue-mem 16384 -metadir $W/meta-lv1-$c -config FlintCatchup$c.cfg FlintCatchupSLiveW.tla > $OUT/$c.out 2>&1
  rc=$?; rm -rf $W/meta-lv1-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*" $OUT/$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  echo "ARMDONE $c" >> $O; }
for c in L17L L17C L17Z L17LNoRec L17CNoRec L17ZNoRec L17LNoRet L17CNoRet L17ZNoRet L17PathSpec L17PathNoRet; do ( arm $c 2400 12 ) & done
until [ $(grep -c "^ARMDONE" $O) -ge 11 ]; do sleep 20; done
echo "JOBDONE $(date -u +%TZ)" >> $O
