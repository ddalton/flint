#!/bin/bash
# One M3 proof iteration (THREADS Z3 threads; 2 at nice 19 in flint-53's slice, 4 once the box was free),
# under the unit's MemoryMax; only the M3 section (line L1 to the end), the
# record run's fingerprints reused for everything before it.
#   systemd-run --user --unit m3proofN -p MemoryMax=24G run-proof-m3.sh N L1 4 0
P=/mnt/nvme/leanp1-m4-2026-10-07/proof
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
n=$1; L1=$2; TH=${3:-2}; NI=${4:-19}
cd $P; mkdir -p out
L2=$(wc -l < LeanP1Proof.tla)
{ echo "M4 RUN $n $(date -u +%Y-%m-%dT%H:%M:%SZ) lines $L1-$L2"; md5sum LeanP1Proof.tla LeanP1Anc.tla; } > out/run$n.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" nice -n $NI $T --toolbox $L1 $L2 --method smt --stretch 12 --threads $TH -k LeanP1Proof.tla >> out/run$n.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/run$n.out
