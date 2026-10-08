#!/bin/bash
# M5 part B's controls (make-controls-b.py), part B's section only, on part
# B's run-1 fingerprints.
#   systemd-run --user --unit m5bctl -p MemoryMax=12G run-ctl-m5b.sh
P=/mnt/nvme/leanp1-m5-2026-10-07/partb
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m5-2026-10-07/controls-b
cd $P
python3 make-controls-b.py LeanP1Proof.tla $C
for c in ctl-gjr ctl-csf; do
  cd $C/$c || continue
  cp $P/LeanP1Anc.tla .
  rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
  cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
  { echo "CONTROL $c $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > $c.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" nice -n 10 $T --toolbox 8050 8153 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $c.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $c.out
done
echo CTLDONE > $C/ctl.done
