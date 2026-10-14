#!/bin/bash
# M6, E43: the other half of M6's controls -- each rule the proof cites, off in
# ScopeHolds, must break its claim in TLC: ConsumeHonorsScope off breaks
# Prop_ScopeRespected; UnlinkChecksBytes off breaks Prop_AgentWorkKept.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e43; mkdir -p $W; cp $D/e43-stage/*.cfg $D/e43-stage/*.tla $W/; cd $W
for n in E43Scope-ConsumeHonorsScope-off E43Scope-UnlinkChecksBytes-off; do
  ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 2h $T -compile -workers 3 -config $W/$n.cfg MCLeanP1M6.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $W/meta $D/cache-$n; done
echo E43DONE > $D/out/e43.done
