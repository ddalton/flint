#!/bin/bash
# The split proof after the udel change (2026-10-13): a record run of each module,
# fingerprints erased, in /mnt/nvme/leanp1-split2 (the regenerated modules).
# the point: the whole-module run reached 28.9 GiB of the box's 30.
#   systemd-run --user --unit m5split -p MemoryMax=28G run-record-split.sh
D=/mnt/nvme/leanp1-split2
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $D; rm -rf .tlacache; mkdir -p out
md5sum *.tla > out/record-md5.txt
for m in M0 M1 M2 M3 M4 M5A M5B; do
  O=out/record-$m.out
  { echo "SPLIT2 RECORD $m $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Proof$m.tla
    echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof$m.tla"; } > $O
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof$m.tla >> $O 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $O
done
echo SPLIT2RECDONE > out/record.done
