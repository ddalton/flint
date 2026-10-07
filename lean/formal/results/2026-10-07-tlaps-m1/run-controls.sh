#!/bin/bash
# Run the three controls on the box, each in its own directory with the record
# run's fingerprints copied in (unchanged obligations are then instant).
P=/mnt/nvme/leanp1-proof-2026-10-06
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m1-2026-10-07/controls
for c in ctl-claim ctl-dwp ctl-ruf; do
  cd $C/$c || exit 1
  rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
  cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/ 2>/dev/null
  md5sum LeanP1Anc.tla LeanP1Proof.tla > $c.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $c.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $c.out
done
echo CTLDONE > $C/ctl.done
