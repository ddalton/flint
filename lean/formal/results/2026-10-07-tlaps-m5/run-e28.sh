#!/bin/bash
# M5 part C, E28: E27 broke NR3 on the edited model -- Edit's ghost base is the
# tree's (stale) BASELINE, so A's second edit k2 does not derive from its own
# published k1.  Candidate (a staging copy, not the module): Edit's base is the
# bytes the agent edited, local[p] when the tree has bytes there, else the
# baseline.  The scripted trace, NR3/AN3 in the five worlds, Prop_NoSilentRevert
# in two, and the MN2/MA1 known-bad runs rebuilt from the candidate.
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
source ~/.cargo/env; W=$D/world-e28; mkdir -p $W; cp $D/e28-stage/*.cfg $D/e28-stage/LeanP1.tla $W/
cp $D/e24-stage/MCLeanP1M5R.tla $D/world-e27/MCLeanP1M5H.tla $W/; cd $W
( timeout 600 $T -workers 2 -config E28Script-NR3.cfg MCLeanP1M5H.tla ) > $D/out/E28Script-NR3.out 2>&1; echo "rc=$?" >> $D/out/E28Script-NR3.out
for m in MN2 MA1; do mkdir -p $D/mut-e28/$m; cp $D/e28-stage/mut/$m/LeanP1.tla $W/MCLeanP1M5R.tla $D/mut-e28/$m/; done
run() { n=$1; dir=$W; case $n in *MN2*) dir=$D/mut-e28/MN2;; *MA1*) dir=$D/mut-e28/MA1;; esac
  ( cd $dir && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M5R.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $dir/meta; }
export -f run; export D W T
printf '%s\n' E28MN2Rem3-NR3 E28MA1Rem3Age-AN3 E28Rem3-NR3 E28Rem3-AN3 E28Scope-NR3 E28Scope-AN3 E28Fetch-NR3 E28Fetch-AN3 \
  E28Fetch-NSR E28Rem3-NSR E28Reader-NR3 E28Reader-AN3 E28Rem3Age-NR3 E28Rem3Age-AN3 | xargs -P 3 -I{} bash -c 'run {}'
echo E28DONE > $D/out/e28.done
