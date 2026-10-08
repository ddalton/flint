#!/bin/bash
# M5's first TLC experiment (E1: Inv_AckedNamed without `h \in live`), queued
# behind M4's small worlds at 2 workers (interpreted tlc-rs).
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
until test -f /mnt/nvme/leanp1-m4-2026-10-07/out/small.done; do sleep 30; done
cd $D/world
run() { name=$1
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n 10 $B -workers 2 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in M5ScopeHoldsProbe M5FetchHolds M5ScopeHolds M5LiveSmall M5ReaderScope; do run $n; done
echo SMALLDONE > $D/out/small.done
