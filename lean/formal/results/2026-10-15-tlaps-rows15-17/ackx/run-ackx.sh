#!/bin/bash
# AK1, AK3, AK4, NR1, DP1, DP2 in the touch worlds the gate lacks: with a
# rescope, a sync, a failed fetch (and rescope + sync), one at a time.
export PATH=$HOME/.cargo/bin:$PATH
D=/mnt/nvme/leanp1-v5/ackx; T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
cd $D; mkdir -p out
for w in "$@"; do
  O=out/$w.out
  ( time TLAMC_CACHE=$D/cache-$w nice -n 10 $T -compile -workers 3 -checkpoint 0 -metadir $D/meta-$w \
      -fpmem 2000 -queue-mem 3000 -config LeanP1$w.cfg LeanP1.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w
done
echo ACKXDONE > out/ackx.done
