#!/bin/bash
# M5 E4b (M5NoBack, M5NoBackY, FlightNoKin over LeanP1Anc): the four small worlds, 2 workers.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
until test -f $D/out/e4.done; do sleep 30; done
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 2 -config $name.cfg MCLeanP1M5Anc.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E4bFetchHolds E4bScopeHolds E4bLiveSmall E4bReaderScope; do run $n; done
echo E4bDONE > $D/out/e4b.done
