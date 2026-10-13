#!/bin/bash
# The split proof (split-proof.py): a record run of each module in turn,
# fingerprints erased, after E11 finishes.  Each module's own peak memory is
# the point: the whole-module run reached 28.9 GiB of the box's 30.
#   systemd-run --user --unit m5split -p MemoryMax=28G run-record-split.sh
D=/mnt/nvme/leanp1-split
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
until [ -f /mnt/nvme/leanp1-m5-2026-10-07/out/e11.done ]; do sleep 60; done
cd $D; rm -rf .tlacache; mkdir -p out
md5sum *.tla > out/md5.txt
for m in M0 M1 M2 M3 M4 M5A M5B; do
  O=out/record-$m.out
  { echo "SPLIT RECORD $m $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Proof$m.tla
    echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof$m.tla"; } > $O
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof$m.tla >> $O 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $O
done
echo SPLITDONE > out/split.done
