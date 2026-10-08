#!/bin/bash
# M5 E6: does Inv_NoRegress ITSELF hold with three gateway removals (rename,
# delete, rename back)?  ScopeHolds with MaxRemovals = 3, copies, restarts
# and rescopes off; the claim alone (MCLeanP1M5 = LeanP1 unchanged).
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 5 $B -workers 3 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
run E6Rem3U; run E6Rem3B
echo E6UDONE > $D/out/e6u.done
