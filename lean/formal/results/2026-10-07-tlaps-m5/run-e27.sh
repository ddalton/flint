#!/bin/bash
# M5 part C, E27: a hand-found NR3 trace on the EDITED model, deeper than any
# world run: A commits k1 at p1 over a renamed-away b0 (conflict <<p1,b0>>),
# edits k2 before Finish (Edit's base: the STALE baseline b0), B takes k1;
# A commits k2 (not foreign: no record); the gateway deletes k2 (udel
# <<p1,k2>>, which does not derive from k1) and renames b0 back; B steps
# k1 -> b0.  Hunt: B a reader; HuntW: B a committing writer.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; mkdir -p $D/world-e27; cp $D/e27-stage/*.cfg $D/world-e27/; cp $D/e24-stage/LeanP1.tla $D/e24-stage/MCLeanP1M5R.tla $D/world-e27/; cd $D/world-e27
run() { n=$1; ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 4h $T -compile -workers 3 -config $n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1; echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; }
run E27Hunt-NR3 & run E27HuntW-NR3 & wait
echo E27DONE > $D/out/e27.done
