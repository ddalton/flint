#!/bin/bash
# M5 part C, E16: TakeOwed (every baseline move onto doc[p] takes an owed path)
# in Rem3, ScopeHolds, FetchHolds (two writers, sync), ReaderScope (the reader).
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; cd $D/world
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 2h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( run E16Rem3-TakeOwed; run E16Scope-TakeOwed ) & ( run E16Fetch-TakeOwed; run E16Reader-TakeOwed ) & wait
echo E16DONE > $D/out/e16.done
