#!/bin/bash
# M5 E4 (M5NoBackX, FlightNoKin over LeanP1Anc): the four small worlds, 2 workers.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 2 -config $name.cfg MCLeanP1M5Anc.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E4FetchHolds E4ScopeHolds E4LiveSmall E4ReaderScope; do run $n; done
echo E4DONE > $D/out/e4.done
