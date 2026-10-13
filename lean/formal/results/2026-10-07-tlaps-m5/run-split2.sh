#!/bin/bash
# The udel change (a commit's delete recorded, 2026-10-13): the split proof
# re-checked module by module, fingerprints CARRIED from leanp1-split (only the
# obligations whose expansion changed -- Install's -- are re-proved).
#   systemd-run --user --unit split2 -p MemoryMax=20G run-split2.sh
D=/mnt/nvme/leanp1-split2; T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $D; mkdir -p out; md5sum *.tla > out/md5.txt
for m in M0 M1 M2 M3 M4 M5A M5B; do
  O=out/check-$m.out
  { echo "SPLIT2 $m $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Proof$m.tla; } > $O
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" nice -n 5 $T --toolbox 0 0 --method smt --stretch 12 --threads 3 -k LeanP1Proof$m.tla >> $O 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $O
done
echo SPLIT2DONE > out/split2.done
