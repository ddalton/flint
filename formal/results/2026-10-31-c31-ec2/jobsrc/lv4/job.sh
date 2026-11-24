# lv4 on c31 (replaces lv3: the arms one after the other, each on the whole machine) (tla-mc 0.2.2): liveness on rev 17 Full at THREE replicas (MaxFails 2, the bounds of the 3-replica full
# passes XLTNJ / XCTNJ), the restated Prop_BarrierEndsM (a barrier ends or the raid has no member; it implies
# Prop_BarrierEndsH since Healthy \subseteq members), lost mark (L17L3M) then crash (L17C3M), through
# FlintCatchupSLiveW2.tla (6604c794).  Submitted after the stamp proof finished, so the arms get the whole machine;
# nice 10 so a proof rerun still comes first.  Each arm ends by 01:35Z (the runner's deadline is 01:40Z).
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/lv4.txt; H=$(pwd); BK=flint-c31-20261031
echo "JOBSTART S $(md5 FlintCatchupS.tla) wrap $(md5 FlintCatchupSLiveW.tla) wrap2 $(md5 FlintCatchupSLiveW2.tla) $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ)" > $O
for c in FlintCatchup*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
[ "$(md5 FlintCatchupS.tla)" = 61ae6829 ] && [ "$(md5 FlintCatchupSLiveW2.tla)" = 6604c794 ] || { echo "MD5 MISMATCH" >> $O; exit 1; }
( while :; do aws s3 sync $OUT s3://$BK/out/jobs/lv4 --quiet; sleep 120; done ) & SY=$!
END=$(date -d 2026-11-01T01:35:00Z +%s)
arm() { c=$1; wk=$2; upto=$3; D=$H/run-$c; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchupSLiveW.tla $H/FlintCatchupSLiveW2.tla $H/FlintCatchup$c.cfg $D/; cd $D; t0=$(date +%s); cap=$(( upto - t0 ))
  echo "ARMSTART $c workers $wk cap ${cap}s $(date -u +%TZ)" >> $O
  timeout $cap nice -n 10 env TLAMC_CACHE=$W/cache-lv4-$c $MC -compile -workers $wk -fpmem 180000 -queue-mem 180000 -checkpoint 0 -metadir $W/meta-lv4-$c -config FlintCatchup$c.cfg FlintCatchupSLiveW2.tla > $OUT/$c.out 2>&1
  rc=$?; rm -rf $W/meta-lv4-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*" $OUT/$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  echo "ARMDONE $c" >> $O; }
# one after the other, 190 workers each: the lost mark first, capped at half the window left; the crash arm gets the rest
MID=$(( $(date +%s) + (END - $(date +%s)) / 2 ))
arm L17L3M 190 $MID
arm L17C3M 190 $END
echo "JOBDONE $(date -u +%TZ)" >> $O
kill $SY 2>/dev/null; aws s3 sync $OUT s3://$BK/out/jobs/lv4 --quiet
