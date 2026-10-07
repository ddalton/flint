#!/bin/bash
# The R3 control needs three rescopes (NOTES.txt): M1Scope3Ctl must FIRE;
# M1Scope3 (the same constants, M1All) runs after the big world is done.
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
run() { name=$1; wk=$2; nc=$3; shift 3
  bin=$D/gen/$name/target/release/tlcgen-mcleanp1m1
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n $nc $bin -workers $wk -config $name.cfg "$@" MCLeanP1M1.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
gen M1Scope3Ctl M1Scope3Ctl.cfg
gen M1Scope3 M1Scope3.cfg
run M1Scope3Ctl 4 10
echo CTL3DONE > $D/out/ctl3.done
until test -f $D/out/big.done; do sleep 60; done
run M1Scope3 8 10
echo SCOPE3DONE > $D/out/scope3.done
