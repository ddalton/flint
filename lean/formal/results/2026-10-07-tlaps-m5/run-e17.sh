#!/bin/bash
# M5 part C, E17: S3Wide (invariant) and TakeOwed2 (step property) together,
# in Rem3, ScopeHolds, FetchHolds, ReaderScope, Rem3Age.  tla-mc 0.1.2 -compile.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; cd $D/world
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( run E17Rem3; run E17Fetch; run E17Rem3Age ) & ( run E17Scope; run E17Reader ) & wait
echo E17DONE > $D/out/e17.done
