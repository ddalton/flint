#!/bin/bash
# M5 part C, E35: the model as decided 2026-10-13 -- Edit's base is the tree's
# newest version at p, the pending upload's snapshot when its file is gone
# (E27, E31, E33) -- and NR5, NR3 with a conflict read by
# derivation (E29).  The four scripted traces; NR5, AN3 in the five worlds;
# Prop_NoSilentRevert in three; MN2 (NR5) and MA1 (AN3) rebuilt from it.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e35; mkdir -p $W; cp $D/e35-stage/*.cfg $D/e35-stage/*.tla $W/; cd $W
for x in 'E27Script-NR5 MCLeanP1M5H' 'E29Script-NR5 MCLeanP1M5H2' 'E31Script-NR5 MCLeanP1M5H3' 'E33Script-NR5 MCLeanP1M5H4'; do set -- $x
  ( timeout 600 $T -workers 1 -config $1.cfg $2.tla ) > $D/out/E35-$1.out 2>&1; echo "rc=$?" >> $D/out/E35-$1.out; done
for m in MN2 MA1; do mkdir -p $D/mut-e35/$m; cp $D/e35-stage/mut/$m/LeanP1.tla $W/MCLeanP1M5R.tla $D/mut-e35/$m/; done
run() { n=$1; dir=$W; case $n in *MN2*) dir=$D/mut-e35/MN2;; *MA1*) dir=$D/mut-e35/MA1;; esac
  ( cd $dir && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $dir/meta; }
export -f run; export D W T
printf '%s\n' E35MN2Rem3-NR5 E35MA1Rem3Age-AN3 E35Rem3-NR5 E35Rem3-AN3 E35Scope-NR5 E35Scope-AN3 E35Fetch-NR5 E35Fetch-AN3 \
  E35Fetch-NSR E35Rem3-NSR E35Scope-NSR E35Reader-NR5 E35Reader-AN3 E35Rem3Age-NR5 E35Rem3Age-AN3 | xargs -P 3 -I{} bash -c 'run {}'
echo E35DONE > $D/out/e35.done
