#!/bin/bash
# M5 E14: the KNOWN-BAD runs that test anything.  E12's MA1/MA2 ran in
# worlds without the retire age (MaxAges = 0), where no handle is ever
# reaped, so `h \in live` names every ack and no mutation can break the
# claim.  Here, in Rem3Age: AN3 (HOLDS unmutated, E10/E11) under MA1 and
# MA2; NR3 (HOLDS unmutated) under MN2, with and without ages.  Each must
# FAIL.  Compiled (tla-mc 5e25b89+a9d204e, -compile), own cache each.
D=/mnt/nvme/leanp1-m5-2026-10-07
T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env
run() { m=$1; name=$2
  ( cd $D/mut/$m && cp $D/world/MCLeanP1M5R.tla . && time TLAMC_CACHE=$D/cache-$name nice -n 10 timeout 3h $T -compile -workers 2 -config $D/world/$name.cfg MCLeanP1M5R.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
run MA1 E14MA1Rem3Age-AN3 & run MA2 E14MA2Rem3Age-AN3 & run MN2 E14MN2Rem3-NR3 & wait
run MN2 E14MN2Rem3Age-NR3
echo E14DONE > $D/out/e14.done
