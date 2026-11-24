# q1 on c31 (tla-mc 0.2.2, 192 cores; rev 17 61ae6829, ZombieTriple, the drain IN: OperatorMark + DrainStamps): the peel of every candidate so far (v7 + I_HeadHiC, I_SnapLegal, I_PendTop, I_PendNext) (every agent
# NEEDS-INV, strong and weak forms side by side; the peel drops each violated one) at 2 replicas full and 4 bounded
md5() { md5sum < $1 | cut -c1-8; }
O=$OUT/q1.txt; H=$(pwd)
true
true
echo "JOBSTART S $(md5 FlintCatchupS.tla) ind $(md5 FlintCatchupSInd.tla) tla-mc $($MC -version 2>&1 | head -1) compiled $(date -u +%TZ) (rev 17, the drain in: every candidate so far)" > $O
for c in FlintCatchup*.cfg; do echo "cfg $c $(md5 $c)" >> $O; done
arm() { c=$1; cap=$2; wk=$3; D=$H/run-$c; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchup$c.cfg $D/; cd $D; t0=$(date +%s)
  timeout $cap env TLAMC_CACHE=$W/cache-q1-$c $MC -compile -workers $wk -fpmem 16384 -queue-mem 16384 -metadir $W/meta-q1-$c -config FlintCatchup$c.cfg FlintCatchupS.tla > $OUT/$c.out 2>&1
  rc=$?; rm -rf $W/meta-q1-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|[Ee]rror:.*" $OUT/$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  echo "ARMDONE $c" >> $O; }
K="J_DueHold J_HoldAtt J_Rel J_Cur J_ZxOut J_Aseq J_SeqSync J_MemRel J_RelHold J_RelHoldS J_NoAckBad J_M0 J_RelValid J_MinorRel J_RelInSync J_ViewIn J_ViewOut J_SoleSeq J_GenBound J_ZGen J_ZxIdle J_SeqLe J_SoleRel J_GenTop J_ZLive J_RelLe TypeOK I_RangesS I_Lineage I_Chain I_Pend I_MembersS I_HoldAtt I_PendSeen I_Vers I_Reverted I_HeadPend I_Session Inv_TrustComplete I_NoBadLoss I_AckedEverS I_LegalS I_LegalS2 I_WHiS I_AckMono I_HeadLoS I_HeadLoA I_HeadLoC I_WHiC I_FreshS I_FreshS2 I_SnapPend I_StandbyLegal Inv_AckedHeld I_HeadHiC I_SnapLegal I_PendTop I_PendNext"
peel() { base=$1; cap0=$2; wk=$3; D=$H/run-$base; mkdir -p $D; cp $H/FlintCatchupS.tla $H/FlintCatchupSInd.tla $H/FlintCatchup$base.cfg $D/; cd $D
  echo "PEEL S $(md5 FlintCatchupS.tla) wrap FlintCatchupSInd.tla $(md5 FlintCatchupSInd.tla) workers $wk base $base cap $cap0 $(date -u +%TZ)" >> $O
  inv=($K); i=0
  while [ ${#inv[@]} -gt 0 ]; do
    i=$((i+1)); c=${base}K$i
    { grep -v -E "^(INVARIANT|PROPERTY|CHECK_DEADLOCK)" FlintCatchup$base.cfg; for v in "${inv[@]}"; do echo "INVARIANT $v"; done; echo "CHECK_DEADLOCK FALSE"; } > FlintCatchup$c.cfg
    t0=$(date +%s)
    timeout $cap0 env TLAMC_CACHE=$W/cache-q1-$base $MC -compile -workers $wk -fpmem 16384 -queue-mem 16384 -metadir $W/meta-q1-$c -config FlintCatchup$c.cfg FlintCatchupSInd.tla > $OUT/$c.out 2>&1
    rc=$?; rm -rf $W/meta-q1-$c
    st=$(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $OUT/$c.out | tail -1)
    bad=$(grep -m1 -oE "Invariant [A-Za-z_0-9]+ is violated" $OUT/$c.out | awk '{print $2}')
    if [ $rc = 0 ]; then echo "PEEL $base HOLD [${inv[*]}] $st $(( $(date +%s) - t0 ))s" >> $O; break
    elif [ -n "$bad" ]; then echo "PEEL $base VIOL $bad $st $(( $(date +%s) - t0 ))s" >> $O; inv=( $(for v in "${inv[@]}"; do [ "$v" != "$bad" ] && echo $v; done) )
    elif [ $rc = 124 ]; then echo "PEEL $base NOVERDICT [${inv[*]}] $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $OUT/$c.out | tail -1)" >> $O; break
    else echo "PEEL $base ERROR rc=$rc $(grep -m1 -iE "error" $OUT/$c.out | cut -c1-160)" >> $O; break; fi
  done
  echo "ARMDONE $base $(date -u +%TZ)" >> $O; }
for e in P17DL:1800:20 P17DC:1800:20 P17DZ:1800:20 P17DCZ:1800:24 P17DL3:1800:20 P17DC3:1800:20 P17DZ3:1800:20 P17DZ4:1800:20; do IFS=: read -r b c w <<< "$e"; ( peel $b $c $w ) & done
until [ $(grep -c "^ARMDONE" $O) -ge 8 ]; do sleep 20; done
echo "JOBDONE $(date -u +%TZ)" >> $O
