#!/bin/bash
# The tie of LeanP1Anc.tla (make-anc.py v4) to LeanP1.tla, rows 15-17 model:
# every world must give EXACTLY the same distinct and generated counts on
# both modules under the gate's cfg (every invariant and action property it
# lists).  The new worlds together take every new step (Touch, Honor,
# RetireByAck, Depose, Fence both arms) and the edited ones.  Both modules
# at the same model; tla-mc 0.1.2 compiled, 4 workers each, two at a time.
export PATH=$HOME/.cargo/bin:$PATH
D=/mnt/nvme/leanp1-anc4; T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
cd $D/world; mkdir -p $D/out; md5sum LeanP1.tla LeanP1Anc.tla > $D/out/md5.txt
run() { w=$1; m=$2
  O=$D/out/$w-$m.out
  ( time TLAMC_CACHE=$D/cache-$w-$m nice -n 5 $T -compile -workers 4 -checkpoint 0 -metadir $D/meta-$w-$m \
      -fpmem 2000 -queue-mem 4000 -config LeanP1$w.cfg $m.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w-$m
}
for w in ReaderHolds DeposeNoAgeHolds FetchHolds ScopeHolds DeposeHolds AckCap ResHolds; do
  run $w LeanP1 & run $w LeanP1Anc & wait
  a=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $D/out/$w-LeanP1.out)
  b=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $D/out/$w-LeanP1Anc.out)
  v=$( [ -n "$a" ] && [ "$a" = "$b" ] && echo EXACT || echo DIFFER )
  echo "$w $v | orig: $a $(grep -h -o -E 'No error has been found|is violated' $D/out/$w-LeanP1.out) | anc: $b $(grep -h -o -E 'No error has been found|is violated' $D/out/$w-LeanP1Anc.out)" >> $D/out/TIE.txt
done
echo TIEDONE >> $D/out/TIE.txt
