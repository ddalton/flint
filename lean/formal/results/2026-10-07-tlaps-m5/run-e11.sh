#!/bin/bash
# M5 E11: (1) AN3 in Rem3, Rem3Age, ScopeHolds; (2) the KNOWN-BAD search --
# each shipped rule off in turn, Inv_AckedNamed and Inv_NoRegress (the
# originals) alone, in FetchHolds and ScopeHolds.  A rule whose loss breaks
# an original is a run every restatement must still fail.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1; wk=$2
  ( time nice -n 10 $B -workers $wk -config $name.cfg MCLeanP1M5R.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
( for w in Rem3 Scope Rem3Age; do run E11$w-AN3 3; done ) &
( for c in E11Scope-*-Inv_*.cfg; do run ${c%.cfg} 2; done ) &
( for c in E11Fetch-*-Inv_*.cfg; do run ${c%.cfg} 2; done ) &
wait
echo E11DONE > $D/out/e11.done
