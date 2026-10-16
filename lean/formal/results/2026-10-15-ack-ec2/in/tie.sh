#!/bin/bash
# The tie on EC2 (user 22:29Z: "run the tie on EC2 after the models finish"):
# LeanP1Anc.tla (make-anc v4, 10166345) vs LeanP1.tla (664f994c), every world
# must give EXACTLY the same generated and distinct counts under the gate cfg.
# The seven worlds of run-tie.sh plus AckRename (L-137's carrier change).
# Waits for runner4's "TLC DONE", stops runner4 (its shutdown would cut this
# off), runs each world's pair side by side at 96 workers, then waits for any
# tlapm, syncs, shuts down.
BK=$1; W=/work; cd $W
export HOME=/root; . /root/.cargo/env
until grep -q '^TLC DONE' out/RESULTS.txt; do sleep 30; done
K=$(pgrep -o -f "[b]ash /work/runner4.sh"); [ -n "$K" ] && kill $K
echo "TIE START $(date -u +%FT%TZ) (runner4 pid $K stopped)" >> out/RESULTS.txt
D=$W/tie; mkdir -p $D/world out/tie; cp $W/formal/*.tla $W/formal/*.cfg $D/world/; cp $W/proof/LeanP1Anc.tla $D/world/
cd $D/world; md5sum LeanP1.tla LeanP1Anc.tla > $W/out/tie/md5.txt
X="-fpmem 32000 -queue-mem 60000"
run() { w=$1; m=$2; O=$W/out/tie/$w-$m.out
  ( time TLAMC_CACHE=$D/cache-$w-$m tla-mc -compile -workers 96 -checkpoint 0 -metadir $D/meta-$w-$m $X -config LeanP1$w.cfg $m.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w-$m $D/cache-$w-$m; }
for w in ReaderHolds DeposeNoAgeHolds FetchHolds ScopeHolds DeposeHolds AckCap ResHolds AckRename; do
  run $w LeanP1 & run $w LeanP1Anc & wait
  a=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $W/out/tie/$w-LeanP1.out)
  b=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $W/out/tie/$w-LeanP1Anc.out)
  v=$( [ -n "$a" ] && [ "$a" = "$b" ] && echo EXACT || echo DIFFER )
  echo "TIE $w $v | orig: $a $(grep -h -o -E 'No error has been found|is violated' $W/out/tie/$w-LeanP1.out) | anc: $b $(grep -h -o -E 'No error has been found|is violated' $W/out/tie/$w-LeanP1Anc.out)" >> $W/out/RESULTS.txt
done
cd $W; echo "TIE DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
while pgrep -x tlapm > /dev/null; do sleep 30; done
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
