#!/bin/bash
# After run-m3.sh: does Inv_ShortcutSound/Inv_ReaderSound need ConsumeHonorsScope?
# (The gate's LeanP1ScopeIgnored world checks only Prop_ScopeRespected.)
D=/mnt/nvme/leanp1-m3-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
until test -f $D/out/small.done; do sleep 30; done
cd $D/world
for name in M3ScopeIgnored; do
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n 19 $B -workers 2 -config $name.cfg MCLeanP1M3.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
done
echo EXTRADONE > $D/out/extra.done
