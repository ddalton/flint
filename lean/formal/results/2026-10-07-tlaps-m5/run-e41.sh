#!/bin/bash
# M5 part D, E41: the restatement AN5 (AN3, or a conflict on the chain by
# derivation, or an acked strict descendant: user's choice 2026-10-14) and the
# proof's strengthening J5D (KD2 /\ CHI /\ BT /\ UD) in the five worlds and on
# the E38/E39 scripts; MA1 is the known-bad run (AN5 and KD2 must break).
D=/mnt/nvme/leanp1-m5-2026-10-07; T=/mnt/nvme/tla-mc-fix-target/release/tla-mc
J=/home/ddalton/lean-gate-2026-09-18/.tla2tools.jar
source ~/.cargo/env; W=$D/world-e41; mkdir -p $W; cp $D/e41-stage/*.cfg $D/e41-stage/*.tla $W/; cd $W
for x in 'E41-E38Script-J5D MCLeanP1M5H5' 'E41-E39Script-J5D MCLeanP1M5H6'; do set -- $x
  ( timeout 600 $T -workers 1 -config $1.cfg $2.tla ) > $D/out/$1.out 2>&1; echo "rc=$?" >> $D/out/$1.out
  ( timeout 600 java -Xmx4g -cp $J tlc2.TLC -workers 1 -config $1.cfg $2.tla ) > $D/out/$1-tlc.out 2>&1; echo "rc=$?" >> $D/out/$1-tlc.out; done
run() { n=$1; dir=$W; case $n in *MA1*) dir=$D/mut-e41; mkdir -p $dir; cp $D/e41-stage/mut/MA1/LeanP1.tla $W/MCLeanP1M5R.tla $W/MCLeanP1M5C.tla $W/MCLeanP1M5D.tla $dir/;; esac
  ( cd $dir && time TLAMC_CACHE=$D/cache-$n nice -n 10 timeout 3h $T -compile -workers 2 -config $W/$n.cfg MCLeanP1M5D.tla ) > $D/out/$n.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$n.out; rm -rf $dir/meta $D/cache-$n; }
export -f run; export D W T
printf '%s\n' E41MA1Rem3Age-AN5 E41MA1Rem3Age-KD2 E41Rem3-AN5 E41Rem3-J5D E41Scope-AN5 E41Scope-J5D E41Fetch-AN5 E41Fetch-J5D \
  E41Reader-AN5 E41Rem3Age-AN5 E41Reader-J5D E41Rem3Age-J5D | xargs -P 3 -I{} bash -c 'run {}'
echo E41DONE > $D/out/e41.done
