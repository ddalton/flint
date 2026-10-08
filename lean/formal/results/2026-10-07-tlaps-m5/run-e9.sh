#!/bin/bash
# M5 E9 (part B's TLC control): Prop_NoSilentRevert in FetchHolds with every
# shipped rule on, then with each rule part B's proof cites switched off --
# GatewayJudgesRead (GCas_Rev), CommitSurfacesForeign (Install_Rev).
D=/mnt/nvme/leanp1-m5-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
cd $D/world
run() { name=$1
  ( time nice -n 10 $B -workers 3 -config $name.cfg MCLeanP1M5.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in E9Fetch-Revert E9Fetch-Revert-GatewayJudgesRead E9Fetch-Revert-CommitSurfacesForeign; do run $n; done
echo E9DONE > $D/out/e9.done
