#!/bin/bash
# TLC pre-check of M2's conjuncts: the two controls and the three small
# worlds now; the three-rescope world and Holds1p3b once M1's worlds are done.
. $HOME/.cargo/env
D=/mnt/nvme/leanp1-m2-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
gen() { name=$1; cfg=$2
  echo "== codegen $name $(date -u +%H:%M:%SZ)" > $D/out/gen-$name.log
  $B -codegen $D/gen/$name -config $cfg MCLeanP1M2.tla >> $D/out/gen-$name.log 2>&1 \
    && (cd $D/gen/$name && cargo build --release >> $D/out/gen-$name.log 2>&1) \
    && echo "== built $(date -u +%H:%M:%SZ)" >> $D/out/gen-$name.log
}
run() { name=$1; wk=$2; nc=$3; shift 3
  bin=$D/gen/$name/target/release/tlcgen-mcleanp1m2
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n $nc $bin -workers $wk -config $name.cfg "$@" MCLeanP1M2.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in M21p3bCtl M21p3bPrivCtl M2ReaderHolds M2FetchHolds M2ScopeHolds M2Scope3 M21p3b; do gen $n $n.cfg; done
run M21p3bCtl 2 10
run M21p3bPrivCtl 2 10
run M2ReaderHolds 2 10 &
run M2FetchHolds 2 10 &
run M2ScopeHolds 2 10 &
wait
echo SMALLDONE > $D/out/small.done
until test -f /mnt/nvme/leanp1-m1-2026-10-07/out/scope3.done; do sleep 60; done
run M2Scope3 8 10
echo SCOPE3DONE > $D/out/scope3.done
run M21p3b 8 15 -checkpoint 30 -metadir $D/meta
echo BIGDONE > $D/out/big.done
