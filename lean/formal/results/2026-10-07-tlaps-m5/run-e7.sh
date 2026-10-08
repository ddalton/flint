#!/bin/bash
# M5 E7: the other two M5 claims, and E1, in E6's three-removal world.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 5 $B -workers 3 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E6Rem3-Inv_AckedNamed E6Rem3-M5NoLive E6Rem3-Revert; do run $n; done
echo E7DONE > $D/out/e7.done
