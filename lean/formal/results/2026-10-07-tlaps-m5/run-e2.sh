#!/bin/bash
# M5 E2 (M5NoBack, Inv_NoRegress's strengthening): the four small worlds,
# then a control sweep over FetchHolds with each shipped rule off in turn.
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 6 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E2FetchHolds E2ScopeHolds E2LiveSmall E2ReaderScope; do run $n; done
for c in E2Fetch-*.cfg; do run ${c%.cfg}; done
echo E2DONE > $D/out/e2.done
