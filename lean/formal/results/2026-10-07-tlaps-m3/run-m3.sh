#!/bin/bash
# TLC pre-check of M3's conjuncts, in the slice flint-53 agreed while it holds
# the box (2 workers, nice 19, MemoryMax=4G): the controls and the small worlds,
# with the interpreted tlc-rs.  M3Scope3 and M31p3b need compiled checkers and
# the whole box; they run later (run-m3-big.sh).
D=/mnt/nvme/leanp1-m3-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n 19 $B -workers 2 -config $name.cfg MCLeanP1M3.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in M3ScopeHoldsAdvCtl M3ScopeHoldsRecordCtl M3ReaderHolds M3FetchHolds M3ScopeHolds M3ReaderScope; do run $n; done
echo SMALLDONE > $D/out/small.done
