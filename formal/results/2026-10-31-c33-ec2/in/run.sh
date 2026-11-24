#!/bin/bash
# c33 on EC2 2026-10-31 (the user, in flint-e4's own session ~21:16Z: "Launch it (Recommended)" for a second spot
# instance, r7i.16xlarge ~$0.67/h, 8 h hard cap, to run the probe/peel queue and tlapm passes in parallel):
# m7i.16xlarge spot, us-east-1f, 64 vCPU / 256 GiB, 300 GB gp3 (21:32Z: us-west-1's spot quota, 192 vCPU, is all c31's;
# r7i.16xlarge was refused MaxSpotInstanceCountExceeded; m7i.16xlarge us-east-1f $0.627/h, the bucket stays in us-west-1).  ONE script, no hand-offs, no bare `wait`;
# userdata's `shutdown -h +480` is the hard cap; this script shuts down 5 min after its final sync.
#  A. at once, in parallel; tla-mc 0.2.1 compiled (the box's batches), every checker in its own copy of its model
#     directory with its own TLAMC_CACHE; caps are clipped to the deadline:
#     d15  the rev 15 drain probe: its 8 arms in parallel, 3 workers each, the box's caps (D15Id 900 s, the rest
#          1200 s); D15Id must count 7,286,102 / d36.  out/d15.txt, the box's batch format.
#     sp5  the v4 candidate set (with I_FreshS) peeled, the box's peel.sh: P1L / P1C / P1Z (2 replicas, cap 1800 s,
#          4 workers; full spaces 3,627,109 / 5,326,386 / 29,140,655 as sp4) and P1L3 / P1C3 / P1Z3 (3 replicas,
#          bounded 600 s, 2 workers as the box), the six bases in parallel.  out/sp5.txt, the box's PEEL format.
#     t1   one tlapm pass on FlintCatchupSProof.tla d7511e2f (fresh cache, stretch 3, 16 threads), the box's sp.sh.
#          out/sp-t1.txt.
#  J. until the deadline (455 min) or s3://BK/inbox/STOP: each s3://BK/inbox/<name>.tgz whose sha256 matches
#     inbox/<name>.sha256 is a job: unpacked into /work/jobs/<name>, its job.sh run in the background (cap: its
#     file `cap`, default 1800 s, clipped to the deadline) with OUT=/work/out/jobs/<name>; out/jobs.txt logs it.
BK=$1; W=/work; F=$W/formal; cd $W; mkdir -p out jobs done
export HOME=/root; . /root/.cargo/env; export CARGO_INCREMENTAL=0
MC=/root/.cargo/bin/tla-mc; TL=$W/tlaps/tlapm/bin/tlapm
T0=$(date +%s); DL=$(( T0 + 455*60 ))
( while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 120; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; df -h $W | tail -1; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l) checkers $(ps -eo args | grep -c '^/work/cache-')"; } >> out/res.txt; sleep 120; done ) & MON=$!
( while sleep 15; do u=$(df --output=pcent / | tail -1 | tr -dc 0-9); [ "$u" -ge 88 ] || continue
    for p in $(ps -eo pid,args | grep "[/]work/cache-[A-Za-z0-9-]*/bin/" | awk '{print $1}'); do
      echo "$(date -u +%TZ) disk ${u}%: cut pid $p" >> $W/out/diskguard.txt; kill $p; done; sleep 90; done ) & DG=$!
md5() { md5sum < $1 | cut -c1-8; }
bye() { echo "$1 $(date -u +%FT%TZ)" >> out/RESULTS.txt; kill $SYNC $MON $DG 2>/dev/null; aws s3 sync $W/out s3://$BK/out --quiet; shutdown -h +5; exit 0; }
clip() { local c=$1 left=$(( DL - $(date +%s) - 60 )); [ $c -gt $left ] && c=$left; echo $c; }
ok=1
for f in "ms15/FlintCatchupS.tla 3d409f1b" "ms15/FlintCatchupD15Id.cfg 2a3a5cde" "ms15/FlintCatchupD15NN.cfg 5f9c7b28" \
         "ms15/FlintCatchupD15NNA.cfg ae7f7258" "ms15/FlintCatchupD15GN.cfg 98f6a046" "ms15/FlintCatchupD15GNA.cfg 29fea7cd" \
         "ms15/FlintCatchupD15NS.cfg afabfc9b" "ms15/FlintCatchupD15GS.cfg bf5b1fb3" "ms15/FlintCatchupD15GSA.cfg 7d92bcd1" \
         "msp4/FlintCatchupS.tla 8e54566d" "msp4/FlintCatchupSInd.tla 697dd704" "msp4/FlintCatchupP1L.cfg 65616b8d" \
         "msp4/FlintCatchupP1C.cfg 24f192e6" "msp4/FlintCatchupP1Z.cfg 38ccaca4" "msp4/FlintCatchupP1L3.cfg a42089ea" \
         "msp4/FlintCatchupP1C3.cfg 9b6c499b" "msp4/FlintCatchupP1Z3.cfg bacc3a92" \
         "proof/FlintCatchupS.tla 8e54566d" "proof/FlintCatchupSInd.tla 697dd704" "proof/FlintCatchupSProof.tla d7511e2f"; do
  set -- $f; m=$(md5 $F/$1); [ "$m" = "$2" ] || { echo "MD5 MISMATCH $1 $m != $2" >> out/RESULTS.txt; ok=0; }
done
[ "$(sha256sum < $W/tlaps.tgz | cut -d' ' -f1)" = "5eaf3aa705b69250afdd53e073ec145c49dfd771d77784a6315fd7d05bad4e44" ] || { echo "TLAPS SHA256 MISMATCH" >> out/RESULTS.txt; ok=0; }
mkdir -p $W/tlaps && tar -xzf $W/tlaps.tgz -C $W/tlaps
v=$($MC -version 2>&1 | head -1); [ "$v" = "tla-mc 0.2.1" ] || { echo "TLA-MC VERSION [$v] != tla-mc 0.2.1" >> out/RESULTS.txt; ok=0; }
[ -x $TL ] || { echo "NO TLAPM at $TL" >> out/RESULTS.txt; ok=0; }
echo "START $(date -u +%FT%TZ) $(nproc) cpus $(free -g | awk '/Mem:/{print $2}')G $v tlapm $($TL --version 2>&1 | head -1) md5ok=$ok" >> out/RESULTS.txt
[ $ok = 1 ] || bye "FATAL inputs"
cat > $W/summ.py <<"PY"
import sys,re
from collections import Counter
t=open(sys.argv[1]).read()
st={}
for bl in re.findall(r"@!!BEGIN\n(.*?)@!!END", t, re.S):
    d=dict(re.findall(r"@!!(\w+):(.*)", bl))
    if "loc" in d and "status" in d: st[d["loc"]]=d["status"]
c=Counter(st.values())
fl=sorted([l for l,s in st.items() if s not in ("proved","trivial")], key=lambda l:[int(n) for n in l.split(":")[:2]])
m=re.findall(r"All \d+ obligations? proved|\d+/\d+ obligations? failed|tlapm ending abnormally.*|[Ee]rror.*|Fatal.*", t)
print("RESULT", dict(c), "|", "; ".join(sorted(set(m))[:5]))
print("FAILED", " ".join(sorted(set(f.split(":")[0] for f in fl), key=int)))
PY
verdict() { # verdict <rc> <out>: the box's wrun2.sh classification; sets v and got
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*|No space left on device" $2 | head -1)
  case "$1:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac; }

# ---- A: d15, sp5, t1 in parallel ----
d15arm() { # d15arm <arm> <cap>
  c=$1; cap=$(clip $2); O=$W/out/d15.txt; D=$W/run-d15-$c; cp -r $F/ms15 $D; cd $D; t0=$(date +%s)
  timeout $cap env TLAMC_CACHE=$W/cache-d15-$c $MC -compile -workers 3 -fpmem 4096 -queue-mem 4096 -metadir $W/meta-d15-$c -config FlintCatchup$c.cfg FlintCatchupS.tla > $W/out/d15-$c.out 2>&1
  rc=$?; rm -rf $W/meta-d15-$c; verdict $rc $W/out/d15-$c.out
  echo "$v $c (FlintCatchupS.tla) rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $W/out/d15-$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $W/out/d15-$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  touch $W/done/d15-$c; }
K="J_DueHold J_HoldAtt J_Rel J_Cur J_ZxOut J_Aseq J_SeqSync J_MemRel J_RelHold J_NoAckBad J_M0 J_RelValid J_MinorRel J_RelInSync J_ViewIn J_ViewOut J_SoleSeq TypeOK I_RangesS I_Lineage I_Chain I_Pend I_Members I_HoldAtt I_PendSeen I_Vers I_Reverted I_HeadPend I_Session Inv_TrustComplete I_NoBadLoss I_AckedEverS I_LegalS I_WHiS I_AckMono I_HeadLoS I_FreshS"
peel() { # peel <base> <cap> <workers>: the box's peel.sh on the v4 set; stops at the first HOLD or a cap
  base=$1; cap0=$2; wk=$3; O=$W/out/sp5.txt; D=$W/run-sp5-$base; cp -r $F/msp4 $D; cd $D
  echo "PEEL tla-mc 0.2.1 S $(md5 FlintCatchupS.tla) wrap FlintCatchupSInd.tla $(md5 FlintCatchupSInd.tla) workers $wk base $base cap $cap0 invs: $K $(date -u +%TZ)" >> $O
  inv=($K); i=0
  while [ ${#inv[@]} -gt 0 ]; do
    i=$((i+1)); c=${base}K$i; cap=$(clip $cap0); [ $cap -lt 120 ] && { echo "PEEL $base SKIPPED (deadline)" >> $O; break; }
    { grep -v -E "^(INVARIANT|PROPERTY|CHECK_DEADLOCK)" FlintCatchup$base.cfg; for v in "${inv[@]}"; do echo "INVARIANT $v"; done; echo "CHECK_DEADLOCK FALSE"; } > FlintCatchup$c.cfg
    t0=$(date +%s)
    timeout $cap env TLAMC_CACHE=$W/cache-sp5-$base $MC -compile -workers $wk -fpmem 4096 -queue-mem 4096 -metadir $W/meta-sp5-$c -config FlintCatchup$c.cfg FlintCatchupSInd.tla > $W/out/sp5-$c.out 2>&1
    rc=$?; rm -rf $W/meta-sp5-$c
    st=$(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $W/out/sp5-$c.out | tail -1)
    bad=$(grep -m1 -oE "Invariant [A-Za-z_0-9]+ is violated" $W/out/sp5-$c.out | awk '{print $2}')
    if [ $rc = 0 ]; then echo "PEEL $base HOLD [${inv[*]}] $st $(( $(date +%s) - t0 ))s" >> $O; break
    elif [ -n "$bad" ]; then echo "PEEL $base VIOL $bad $st $(( $(date +%s) - t0 ))s" >> $O; inv=( $(for v in "${inv[@]}"; do [ "$v" != "$bad" ] && echo $v; done) )
    elif [ $rc = 124 ]; then echo "PEEL $base NOVERDICT [${inv[*]}] $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $W/out/sp5-$c.out | tail -1)" >> $O; break
    else echo "PEEL $base ERROR rc=$rc $(grep -m1 -iE "error" $W/out/sp5-$c.out | cut -c1-120)" >> $O; break; fi
  done
  echo "PEELDONE $base $(date -u +%TZ)" >> $O; touch $W/done/sp5-$base; }
t1() { O=$W/out/sp-t1.txt; D=$W/run-t1; cp -r $F/proof $D; cd $D
  echo "S $(md5 FlintCatchupS.tla) ind $(md5 FlintCatchupSInd.tla) proof $(md5 FlintCatchupSProof.tla) tlapm bfa9468 start $(date -u +%TZ) threads 16" > $O
  t0=$(date +%s)
  timeout $(clip 14400) /usr/bin/time -v -o $W/out/sp-t1.time $TL --toolbox 0 0 --cleanfp --stretch 3 --threads 16 FlintCatchupSProof.tla > $W/out/sp-t1.out 2>&1
  rc=$?; python3 $W/summ.py $W/out/sp-t1.out >> $O
  echo "DONE rc=$rc secs=$(( $(date +%s) - t0 )) maxrss=$(grep -oP "Maximum resident set size \(kbytes\): \K\d+" $W/out/sp-t1.time)KB $(date -u +%TZ)" >> $O
  touch $W/done/t1; }
echo "D15 S $(md5 $F/ms15/FlintCatchupS.tla) tla-mc 0.2.1 compiled m7i.16xlarge workers 3 fpmem/queue-mem 4096 arms in parallel $(date -u +%TZ)" > out/d15.txt
NA=0
( t1 ) & NA=$((NA+1))
for e in D15Id:900 D15NN:1200 D15NNA:1200 D15GN:1200 D15GNA:1200 D15NS:1200 D15GS:1200 D15GSA:1200; do ( d15arm ${e%%:*} ${e#*:} ) & NA=$((NA+1)); done
for e in P1L:1800:4 P1C:1800:4 P1Z:1800:4 P1L3:600:2 P1C3:600:2 P1Z3:600:2; do IFS=: read -r b c w <<< "$e"; ( peel $b $c $w ) & NA=$((NA+1)); done
echo "PHASE A started: $NA items $(date -u +%TZ)" >> out/RESULTS.txt

# ---- J: the inbox ----
adone=0
while :; do
  [ $(date +%s) -ge $DL ] && { echo "DEADLINE $(date -u +%TZ)" >> out/RESULTS.txt; break; }
  [ $adone = 0 ] && [ $(ls $W/done | wc -l) -ge $NA ] && { adone=1; echo "PHASE A DONE $(date -u +%TZ)" >> out/RESULTS.txt; aws s3 sync $W/out s3://$BK/out --quiet; }
  aws s3 ls s3://$BK/inbox/ 2>/dev/null | awk '{print $4}' > $W/inbox.ls
  grep -qx STOP $W/inbox.ls && { echo "STOP $(date -u +%TZ)" >> out/RESULTS.txt; break; }
  for n in $(grep '\.tgz$' $W/inbox.ls | sed 's/\.tgz$//'); do
    [ -e $W/jobs/$n ] && continue; mkdir -p $W/jobs/$n $W/out/jobs/$n
    aws s3 cp s3://$BK/inbox/$n.sha256 $W/jobs/$n.sha256 --quiet; aws s3 cp s3://$BK/inbox/$n.tgz $W/jobs/$n.tgz --quiet
    [ "$(sha256sum < $W/jobs/$n.tgz | cut -d' ' -f1)" = "$(cut -d' ' -f1 $W/jobs/$n.sha256 2>/dev/null)" ] || { echo "JOB $n SHA MISMATCH $(date -u +%TZ)" >> out/jobs.txt; continue; }
    tar -xzf $W/jobs/$n.tgz -C $W/jobs/$n
    cap=$(clip $(cat $W/jobs/$n/cap 2>/dev/null || echo 1800)); [ $cap -lt 60 ] && { echo "JOB $n SKIPPED (deadline)" >> out/jobs.txt; continue; }
    echo "JOB $n START cap ${cap}s $(date -u +%TZ)" >> out/jobs.txt
    ( cd $W/jobs/$n; timeout $cap env OUT=$W/out/jobs/$n MC=$MC TL=$TL SUMM=$W/summ.py W=$W bash job.sh > $W/out/jobs/$n/job.log 2>&1
      echo "JOB $n DONE rc=$? $(date -u +%TZ)" >> $W/out/jobs.txt; aws s3 sync $W/out s3://$BK/out --quiet ) &
  done
  sleep 20
done
for d in t1 $(echo d15-D15Id d15-D15NN d15-D15NNA d15-D15GN d15-D15GNA d15-D15NS d15-D15GS d15-D15GSA sp5-P1L sp5-P1C sp5-P1Z sp5-P1L3 sp5-P1C3 sp5-P1Z3); do
  [ -e $W/done/$d ] || echo "CUT $d (not finished at the end)" >> out/RESULTS.txt; done
bye "DONE"
