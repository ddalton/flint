#!/bin/bash
# M5 E8: E6's world with the retire age (MaxAges = 1): the sweep can take.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 5 $B -workers 3 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E6Rem3Age-Revert; do run $n; done
echo E8bDONE > $D/out/e8b.done
