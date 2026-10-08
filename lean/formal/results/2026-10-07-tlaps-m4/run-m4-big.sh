#!/bin/bash
# M41p3b (compiled checker), after the m3big chain (M2's M21p3b resumed, then
# M31p3b) has finished.
. $HOME/.cargo/env
D=/mnt/nvme/leanp1-m4-2026-10-07
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
until test -f /mnt/nvme/leanp1-m3-2026-10-07/out/big.done; do sleep 60; done
name=M41p3b
echo "== codegen $name $(date -u +%H:%M:%SZ)" > $D/out/gen-$name.log
(cd $D/world && $B -codegen $D/gen/$name -config $name.cfg MCLeanP1M4.tla) >> $D/out/gen-$name.log 2>&1 \
  && (cd $D/gen/$name && nice -n 10 cargo build --release -j 4 >> $D/out/gen-$name.log 2>&1) \
  && echo "== built $(date -u +%H:%M:%SZ)" >> $D/out/gen-$name.log
mkdir -p $D/meta
date -u +%H:%M:%SZ > $D/out/$name.start
( cd $D/world && time $D/gen/$name/target/release/tlcgen-mcleanp1m4 -workers 8 -config $name.cfg -checkpoint 30 -metadir $D/meta MCLeanP1M4.tla ) > $D/out/$name.out 2>&1
echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
echo BIGDONE > $D/out/big.done
