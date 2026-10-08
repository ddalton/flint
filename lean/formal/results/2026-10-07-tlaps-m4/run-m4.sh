#!/bin/bash
# M4 on the box, started once M3's record run has ended (its six controls
# then run beside this): the TLC pre-check at 2 workers (interpreted tlc-rs),
# and the first proof run over the M4 section at 2 threads, the M3 record's
# fingerprints reused for everything before it.
D=/mnt/nvme/leanp1-m4-2026-10-07
P3=/mnt/nvme/leanp1-m3-2026-10-07/proof
B=/mnt/nvme/leanp1-ind-2026-10-05/src/formal/tlc-rs/target/release/tlc-rs
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
until grep -q PROOFDONE $P3/out/runrecord.out 2>/dev/null; do sleep 30; done
# The proof run, in the background.
mkdir -p $D/proof/.tlacache/LeanP1Proof.tlaps $D/proof/out
cp $P3/.tlacache/LeanP1Proof.tlaps/fingerprints $D/proof/.tlacache/LeanP1Proof.tlaps/
( cd $D/proof
  L1=$(( $(grep -n "(\* M4: Inv_ReaderFetches" LeanP1Proof.tla | cut -d: -f1) - 2 )); L2=$(wc -l < LeanP1Proof.tla)
  { echo "M4 RUN 1 $(date -u +%Y-%m-%dT%H:%M:%SZ) lines $L1-$L2"; md5sum LeanP1Proof.tla LeanP1Anc.tla; } > out/run1.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox $L1 $L2 --method smt --stretch 12 --threads 2 -k LeanP1Proof.tla >> out/run1.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/run1.out ) &
cd $D/world
run() { name=$1
  date -u +%H:%M:%SZ > $D/out/$name.start
  ( time nice -n 10 $B -workers 2 -config $name.cfg MCLeanP1M4.tla ) > $D/out/$name.out 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $D/out/$name.out
}
for n in M4LiveSmallCitedCtl M4LiveSmallAgedCtl M4ReaderLoses M4LiveSmall M4ReaderHolds M4FetchHolds M4ScopeHolds M4ReaderScope; do run $n; done
echo SMALLDONE > $D/out/small.done
wait
echo M4DONE > $D/out/m4.done
