#!/bin/bash
# TLC pre-check of M1's new conjuncts (MCLeanP1M1.tla over LeanP1.tla unchanged):
# the control first, the three small worlds in parallel, then Holds1p3b.
. $HOME/.cargo/env
D=/mnt/nvme/leanp1-m1-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
gen() { name=$1; cfg=$2
  echo "== codegen $name $(date -u +%H:%M:%SZ)" > $D/out/gen-$name.log
  $B -codegen $D/gen/$name -config $cfg MCLeanP1M1.tla >> $D/out/gen-$name.log 2>&1 \
    && (cd $D/gen/$name && cargo build --release >> $D/out/gen-$name.log 2>&1) \
    && echo "== built $(date -u +%H:%M:%SZ)" >> $D/out/gen-$name.log
}
for n in M1ScopeCtl M1ReaderHolds M1FetchHolds M1ScopeHolds M11p3b; do gen $n $n.cfg; done
run() { name=$1; wk=$2; nc=$3; shift 3
  bin=$D/gen/$name/target/release/tlcgen-mcleanp1m1
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n $nc $bin -workers $wk -config $name.cfg "$@" MCLeanP1M1.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
run M1ScopeCtl 2 10
run M1ReaderHolds 2 10 &
run M1FetchHolds 2 10 &
run M1ScopeHolds 2 10 &
wait
echo SMALLDONE > $D/out/small.done
run M11p3b 8 15 -checkpoint 30 -metadir $D/meta
echo BIGDONE > $D/out/big.done
