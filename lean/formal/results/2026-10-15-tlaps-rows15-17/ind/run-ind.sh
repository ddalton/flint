#!/bin/bash
# The old proof's inductive conjuncts on the rows 15-17 model (MCLeanP1Ind.tla):
# usage: run-ind.sh WORLD WORKERS -- every conjunct an INVARIANT; the first
# violated one is reported (tla-mc stops there).
export PATH=$HOME/.cargo/bin:$PATH
D=/mnt/nvme/leanp1-v5/ind; T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
w=$1; n=${2:-2}; cd $D; mkdir -p out
O=out/Ind$w.out
( time TLAMC_CACHE=$D/cache-$w nice -n 15 $T -compile -workers $n -checkpoint 0 -metadir $D/meta-$w \
    -fpmem 2000 -queue-mem 3000 -config Ind$w.cfg MCLeanP1Ind.tla ) > $O 2>&1
echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w
