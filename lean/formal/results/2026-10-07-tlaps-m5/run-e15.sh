#!/bin/bash
# M5 part C, E15: NR3's strengthening candidates S3All, S3Owed in Rem3, Rem3Age,
# ScopeHolds.  tla-mc 0.1.2 -compile, own cache each, 2 workers; beside the
# split proof's record runs.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; cd $D/world
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 2h $T -compile -workers 2 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
( for w in Rem3 Scope Rem3Age; do run E15$w-S3All; done ) &
( for w in Rem3 Scope Rem3Age; do run E15$w-S3Owed; done ) &
wait; echo E15DONE > $D/out/e15.done
