#!/bin/bash
# ZRS: the gateway DocCar question.  One script; setsid: stop it with kill -- -PGID.
cd /mnt/nvme/leanp1-v6/m10g; export PATH=$HOME/.cargo/bin:$PATH
T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
w=ZRS; rm -rf cache-$w meta-$w
( time timeout --foreground 10800 env TLAMC_CACHE=$PWD/cache-$w nice -n 10 $T -compile -workers 3 -checkpoint 0 -metadir $PWD/meta-$w -fpmem 6000 -queue-mem 8000 -config $w.cfg MCLeanP1M10.tla ) > $w.out 2>&1
echo "rc=$? $(date -u +%H:%M:%SZ)" >> $w.out; rm -rf meta-$w
