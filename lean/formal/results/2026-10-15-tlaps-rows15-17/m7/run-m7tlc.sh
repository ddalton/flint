#!/bin/bash
# M7's candidates (MCLeanP1M7.tla) on the gate's ack worlds, after the ackx runs.
export PATH=$HOME/.cargo/bin:$PATH
D=/mnt/nvme/leanp1-v5/m7; T=/mnt/nvme/tla-mc-0.1.2/bin/tla-mc
cd $D; mkdir -p out
while [ ! -f ../ackx/out/ackx.done ]; do sleep 20; done
for w in "$@"; do
  O=out/M7$w.out
  ( time TLAMC_CACHE=$D/cache-$w nice -n 10 $T -compile -workers 3 -checkpoint 0 -metadir $D/meta-$w \
      -fpmem 2000 -queue-mem 3000 -config M7$w.cfg MCLeanP1M7.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w
done
echo M7DONE > out/m7.done
