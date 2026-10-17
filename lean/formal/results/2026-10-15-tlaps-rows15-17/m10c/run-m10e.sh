#!/bin/bash
# M11's inductive candidates (MidSnap, LostUncited = Q1, CFetch = Q2) with the
# M10 set, on the FIXED model.  One script, sequential, 7 workers; setsid: stop it with kill -- -PGID.
cd /mnt/nvme/leanp1-v6/m10d; export PATH=$HOME/.cargo/bin:$PATH
T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
for w in NM10A NM10C1 NM10C2 NM10AckRescopeRename2 NM10B; do
  rm -rf cache-$w meta-$w
  ( time timeout 5400 env TLAMC_CACHE=$PWD/cache-$w nice -n 5 $T -compile -workers 7 -checkpoint 0 -metadir $PWD/meta-$w -fpmem 6000 -queue-mem 8000 -config $w.cfg MCLeanP1M10.tla ) > $w.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $w.out; rm -rf meta-$w
done
echo DONE > M10E-DONE
