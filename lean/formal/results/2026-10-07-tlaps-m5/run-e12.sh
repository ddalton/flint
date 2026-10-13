#!/bin/bash
# M5 E12: the mutants (make-mutants.py) and the three-path world.  Each
# mutant runs in its own directory (its LeanP1.tla is the mutation).
# The P3 world runs unmutated, each check capped at 2 h (`timeout`).
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
run() { dir=$1; name=$2; wk=$3; cap=$4
  ( cd $dir && time nice -n 10 timeout $cap $B -workers $wk -config $D/world/$name.cfg MCLeanP1M5R.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
cd $D/world && python3 make-mutants.py $D/mut
for m in MA1 MA2 MN2; do cp $D/world/MCLeanP1M5R.tla $D/mut/$m/; done
( for m in MA1 MA2; do for w in Scope Rem3; do for c in Inv_AckedNamed AN1 AN2 AN3; do run $D/mut/$m E12$m$w-$c 2 2h; done; done; done ) &
( for w in Rem3 Rem3Age; do for c in Inv_NoRegress NR0 NR2; do run $D/mut/MN2 E12MN2$w-$c 2 2h; done; done ) &
( for c in Inv_NoRegress NR2 AN3 Inv_AckedNamed; do run $D/world E12P3-$c 2 2h; done ) &
wait
echo E12DONE > $D/out/e12.done
