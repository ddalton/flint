#!/bin/bash
# M5 E10: the restatement candidates (MCLeanP1M5R.tla) and their calibrations,
# one claim per run, in the two three-removal worlds (Rem3, Rem3Age) and the
# gate's ScopeHolds.  Two lanes of 3 workers.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 3 -config $name.cfg MCLeanP1M5R.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
C="Inv_NoRegress NR0 NR1 NR2 Inv_AckedNamed AN0 AN1 AN2"
lane() { for w in "$@"; do for c in $C; do run E10$w-$c; done; done; }
lane Rem3 Scope & lane Rem3Age & wait
echo E10DONE > $D/out/e10.done
