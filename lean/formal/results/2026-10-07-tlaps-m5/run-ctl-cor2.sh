#!/bin/bash
# ctl-cor run 2: the control regenerated with Upload_TypeOK's <2>3 following
# the mutation (run 1 left it at Content(h); its <2>7 failed for that), on
# the record run's fingerprints.
#   systemd-run --user --unit m5ctlcor2 -p MemoryMax=12G run-ctl-cor2.sh
P=/mnt/nvme/leanp1-m5-2026-10-07/proof
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m5-2026-10-07/controls2
cd $P
python3 make-controls.py LeanP1Proof.tla $C
cd $C/ctl-cor
rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
{ echo "CONTROL ctl-cor run 2 $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > ctl-cor.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> ctl-cor.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> ctl-cor.out
echo CTLDONE > $C/ctl.done
