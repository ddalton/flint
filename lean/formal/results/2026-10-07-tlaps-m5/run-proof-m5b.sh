#!/bin/bash
# M5 part B: Prop_NoSilentRevert.  The part-B section only, on part A's
# record fingerprints (copied) for everything before it.  Run N: $1.
#   systemd-run --user --unit m5b -p MemoryMax=12G run-proof-m5b.sh N
D=/mnt/nvme/leanp1-m5-2026-10-07/partb
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $D
if [ ! -d .tlacache ]; then
  mkdir -p .tlacache/LeanP1Proof.tlaps
  cp /mnt/nvme/leanp1-m5-2026-10-07/proof/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
fi
O=out/proofb-run$1.out; mkdir -p out
{ echo "M5 PART B run $1 $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 8050 8153 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla"; } > $O
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" nice -n 10 $T --toolbox 8050 8153 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $O 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $O
