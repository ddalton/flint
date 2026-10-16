#!/bin/bash
# CPubLeft in the rescope worlds (RUL, RUL2), sequential, 2 workers.  One script.
cd /mnt/nvme/leanp1-v6/m10h || exit 1; export PATH=$HOME/.cargo/bin:$PATH
T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
for w in RUL RUL2; do
  rm -rf cache-$w meta-$w
  ( time timeout --foreground 7200 env TLAMC_CACHE=$PWD/cache-$w nice -n 10 $T -compile -workers 2 -checkpoint 0 -metadir $PWD/meta-$w -fpmem 4000 -queue-mem 4000 -config $w.cfg MCLeanP1M10.tla ) > $w.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $w.out; rm -rf meta-$w
done
echo DONE > RUL-DONE
