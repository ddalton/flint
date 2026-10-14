#!/bin/bash
# M5 part D, E37: which of AN3's names its proof can do without (MCLeanP1M5D.tla:
# A1 no live disjunct, A2 no tombstone, A3 no tree memory, A4 no later ack; TU
# a tombstone is an acked delete or a rename's source) in the five worlds; MA1
# is the known-bad run (A1 must break).
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e37; mkdir -p $W; cp $D/e37-stage/*.cfg $D/e37-stage/*.tla $W/; cd $W
run() { n=$1; dir=$W; case $n in *MA1*) dir=$D/mut-e37; mkdir -p $dir; cp $D/e37-stage/mut/MA1/LeanP1.tla $W/MCLeanP1M5R.tla $W/MCLeanP1M5C.tla $W/MCLeanP1M5D.tla $dir/;; esac
  ( cd $dir && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M5D.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $dir/meta $D/cache-$n; }
export -f run; export D W T
{ echo E37MA1Rem3Age-A1; for c in A1 A2 A3 A4 TU; do for wld in Rem3 Scope Fetch; do echo E37$wld-$c; done; done
  for c in A1 A2 A3 A4 TU; do echo E37Reader-$c; echo E37Rem3Age-$c; done; } | xargs -P 3 -I{} bash -c 'run {}'
echo E37DONE > $D/out/e37.done
