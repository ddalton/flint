#!/bin/bash
# The worlds that need compiled checkers, once flint-53 freed the box (21:07Z),
# at 4 workers beside the proof runs: M3Scope3; M2's M21p3b resumed from its
# depth-29 checkpoint (stopped 20:13Z for flint-53); M31p3b.
. $HOME/.cargo/env
D=/mnt/nvme/leanp1-m3-2026-10-07
D2=/mnt/nvme/leanp1-m2-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
gen() { name=$1
  echo "== codegen $name $(date -u +%H:%M:%SZ)" > $D/out/gen-$name.log
  (cd $D/world && $B -codegen $D/gen/$name -config $name.cfg MCLeanP1M3.tla) >> $D/out/gen-$name.log 2>&1 \
    && (cd $D/gen/$name && nice -n 10 cargo build --release -j 4 >> $D/out/gen-$name.log 2>&1) \
    && echo "== built $(date -u +%H:%M:%SZ)" >> $D/out/gen-$name.log
}
run() { name=$1; shift
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( cd $D/world && time $D/gen/$name/target/release/tlcgen-mcleanp1m3 -workers 4 -config $name.cfg "$@" MCLeanP1M3.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
mkdir -p $D/gen
gen M3Scope3; run M3Scope3
echo SCOPE3DONE > $D/out/scope3.done
# M2's big world, resumed.
date -u +%H:%M:%SZ > $D2/out/M21p3b-recover.start
( cd $D2/world && time $D2/gen/M21p3b/target/release/tlcgen-mcleanp1m2 -workers 4 -config M21p3b.cfg -checkpoint 30 -recover $D2/meta MCLeanP1M2.tla ) > $D2/out/M21p3b-recover.out 2>&1
echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D2/out/M21p3b-recover.out
echo BIGDONE > $D2/out/big.done
gen M31p3b; mkdir -p $D/meta; run M31p3b -checkpoint 30 -metadir $D/meta
echo BIGDONE > $D/out/big.done
