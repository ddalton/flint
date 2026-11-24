# pk1: the v5 peel on rev 15 with the in-write rule (the box's peel.sh, as run.sh's peel()): seven bases in parallel
md5() { md5sum < $1 | cut -c1-8; }
K="J_DueHold J_HoldAtt J_Rel J_Cur J_ZxOut J_Aseq J_SeqSync J_MemRel J_RelHold J_NoAckBad J_M0 J_RelValid J_MinorRel J_RelInSync J_ViewIn J_ViewOut J_SoleSeq TypeOK I_RangesS I_Lineage I_Chain I_Pend I_Members I_HoldAtt I_PendSeen I_Vers I_Reverted I_HeadPend I_Session Inv_TrustComplete I_NoBadLoss I_AckedEverS I_LegalS I_WHiS I_AckMono I_HeadLoS I_FreshS J_GenBound J_ZGen"
O=$OUT/peel.txt; H=$(pwd)
echo "JOBSTART S $(md5 FlintCatchupS.tla) ind $(md5 FlintCatchupSInd.tla) tla-mc $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ)" > $O
for c in FlintCatchupP15*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
peel() { base=$1; cap0=$2; wk=$3; D=$H/run-$base; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchupSInd.tla $H/FlintCatchup$base.cfg $D/; cd $D
  echo "PEEL S $(md5 FlintCatchupS.tla) wrap FlintCatchupSInd.tla $(md5 FlintCatchupSInd.tla) workers $wk base $base cap $cap0 $(date -u +%TZ)" >> $O
  inv=($K); i=0
  while [ ${#inv[@]} -gt 0 ]; do
    i=$((i+1)); c=${base}K$i
    { grep -v -E "^(INVARIANT|PROPERTY|CHECK_DEADLOCK)" FlintCatchup$base.cfg; for v in "${inv[@]}"; do echo "INVARIANT $v"; done; echo "CHECK_DEADLOCK FALSE"; } > FlintCatchup$c.cfg
    t0=$(date +%s)
    timeout $cap0 env TLAMC_CACHE=$W/cache-pk1-$base $MC -compile -workers $wk -fpmem 4096 -queue-mem 4096 -metadir $W/meta-pk1-$c -config FlintCatchup$c.cfg FlintCatchupSInd.tla > $OUT/$c.out 2>&1
    rc=$?; rm -rf $W/meta-pk1-$c
    st=$(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1)
    bad=$(grep -m1 -oE "Invariant [A-Za-z_0-9]+ is violated" $OUT/$c.out | awk '{print $2}')
    if [ $rc = 0 ]; then echo "PEEL $base HOLD [${inv[*]}] $st $(( $(date +%s) - t0 ))s" >> $O; break
    elif [ -n "$bad" ]; then echo "PEEL $base VIOL $bad $st $(( $(date +%s) - t0 ))s" >> $O; inv=( $(for v in "${inv[@]}"; do [ "$v" != "$bad" ] && echo $v; done) )
    elif [ $rc = 124 ]; then echo "PEEL $base NOVERDICT [${inv[*]}] $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1)" >> $O; break
    else echo "PEEL $base ERROR rc=$rc $(grep -m1 -iE "error" $OUT/$c.out | cut -c1-160)" >> $O; break; fi
  done
  echo "PEELDONE $base $(date -u +%TZ)" >> $O; }
for e in P15L:1500:6 P15C:1500:6 P15Z:1500:6 P15CZ:1500:8 P15L3:600:2 P15C3:600:2 P15Z3:600:2; do IFS=: read -r b c w <<< "$e"; ( peel $b $c $w ) & done
until [ $(grep -c "^PEELDONE" $O) -ge 7 ]; do sleep 20; done
echo "JOBDONE $(date -u +%TZ)" >> $O
