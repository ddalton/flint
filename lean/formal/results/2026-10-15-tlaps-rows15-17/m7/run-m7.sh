#!/bin/bash
# The L-137 model (2026-10-15): run-tlc6.sh WORKERS WORLD[:MUTANTDIR]...  Each world's cfg
# in this dir; a mutant's module in MUTANTDIR/LeanP1.tla.  30-minute cap each.
export PATH=$HOME/.cargo/bin:$PATH
D=/mnt/nvme/leanp1-v6/m7; T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc; n=$1; shift
cd $D; mkdir -p out
for spec in "$@"; do
  w=${spec%%:*}; m=${spec#*:}; [ "$m" = "$spec" ] && m=.
  cp M7$w.cfg $m/ 2>/dev/null
  O=$D/out/$w.out
  ( cd $m; time timeout 1800 env TLAMC_CACHE=$D/cache-$w nice -n 10 $T -compile -workers $n -checkpoint 0 -metadir $D/meta-$w \
      -fpmem 2000 -queue-mem 3000 -config M7$w.cfg MCLeanP1M7.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w
done
echo DONE >> $D/out/done-$$.txt
