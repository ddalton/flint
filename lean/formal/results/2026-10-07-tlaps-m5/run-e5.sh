#!/bin/bash
# M5 E5: E1 with one more disjunct dropped, each variant in the four small
# worlds (2 workers, after E3).  A variant that FAILS marks a disjunct the
# claim needs; one that HOLDS marks one the proof need not maintain.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
until test -f $D/out/e3.done; do sleep 30; done
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 2 -config $name.cfg MCLeanP1M5E5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for v in NoTomb NoTook NoUdel NoLater NoPres; do
  for w in FetchHolds ScopeHolds LiveSmall ReaderScope; do run E5$w-$v; done
done
echo E5DONE > $D/out/e5.done
