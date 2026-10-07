#!/bin/bash
# ctl-cag again, after the chain: its first text deleted Install's <1>s step
# that later steps cite (tlapm: Operator "<1>s" not found).
P=/mnt/nvme/leanp1-m3-2026-10-07/proof
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m3-2026-10-07/controls
until test -f $C/ctl.done; do sleep 30; done
cd $C/ctl-cag
mv ctl-cag.out ctl-cag-parse-error.out
rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
{ echo "CONTROL ctl-cag (rerun) $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > ctl-cag.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> ctl-cag.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> ctl-cag.out
echo CAGDONE > $C/cag.done
