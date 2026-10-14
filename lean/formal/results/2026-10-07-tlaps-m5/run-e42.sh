#!/bin/bash
# M6, E42: J6 (MCLeanP1M6.tla) with Prop_AgentWorkKept and Prop_ScopeRespected,
# in the five worlds, on the final model.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e42; mkdir -p $W; cp $D/e42-stage/*.cfg $D/e42-stage/*.tla $W/; cd $W
run() { n=$1
  ( time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M6.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $W/meta $D/cache-$n; }
export -f run; export D W T
printf '%s\n' E42Scope-J6 E42Fetch-J6 E42Rem3-J6 E42Reader-J6 E42Rem3Age-J6 | xargs -P 3 -I{} bash -c 'run {}'
echo E42DONE > $D/out/e42.done
