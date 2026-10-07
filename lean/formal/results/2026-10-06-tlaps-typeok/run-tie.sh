#!/bin/bash
. $HOME/.cargo/env
D=/mnt/nvme/leanp1-anc3-2026-10-06
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
gen() { name=$1; mod=$2; cfg=$3
  echo "== codegen $name $(date -u +%H:%M:%SZ)" > $D/out/gen-$name.log
  $B -codegen $D/gen/$name -config $cfg $mod.tla >> $D/out/gen-$name.log 2>&1 && (cd $D/gen/$name && cargo build --release >> $D/out/gen-$name.log 2>&1) && echo "== built $(date -u +%H:%M:%SZ)" >> $D/out/gen-$name.log
}
gen TieHolds1p3b LeanP1Anc TieHolds1p3b.cfg
gen CheckReaderHolds MCLeanP1AncCheck CheckReaderHolds.cfg
gen CheckFetchHolds MCLeanP1AncCheck CheckFetchHolds.cfg
gen CheckScopeHolds MCLeanP1AncCheck CheckScopeHolds.cfg
run() { name=$1; mod=$2; cfg=$3; wk=$4; shift 4
  bin=$D/gen/$name/target/release/tlcgen-$(echo $mod | tr A-Z a-z)
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n 10 $bin -workers $wk -config $cfg "$@" $mod.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
run CheckReaderHolds MCLeanP1AncCheck CheckReaderHolds.cfg 2 &
run CheckFetchHolds MCLeanP1AncCheck CheckFetchHolds.cfg 2 &
run CheckScopeHolds MCLeanP1AncCheck CheckScopeHolds.cfg 2 &
wait
echo SMALLDONE > $D/out/small.done
run TieHolds1p3b LeanP1Anc TieHolds1p3b.cfg 8 -checkpoint 30 -metadir $D/meta
echo TIEDONE > $D/out/tie.done
