#!/bin/bash
# 2026-10-14: the WHOLE proof re-recorded after NR5 and AN5 moved into
# LeanP1.tla (LeanP1Anc.tla regenerated; M5C and M5D drop their copies of the
# claims).  Every module in turn, fingerprints erased: the split M0-M5B
# (split-proof.py), M5C, M5D, M6.  tlapm checks only the module it is given.
D=/mnt/nvme/leanp1-full; T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $D; rm -rf .tlacache; mkdir -p out; md5sum *.tla > out/md5.txt
for m in M0 M1 M2 M3 M4 M5A M5B M5C M5D M6; do
  O=out/record-$m.out
  { echo "FULL RECORD $m $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Proof$m.tla
    echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1Proof$m.tla (MemoryMax=26G)"; } > $O
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" systemd-run --user --scope -p MemoryMax=26G nice -n 5 \
    $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1Proof$m.tla >> $O 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $O
done
echo FULLDONE > out/full.done
