#!/bin/bash
# M6: check LeanP1ProofM6.tla (EXTENDS LeanP1ProofM5D; tlapm checks only
# this module's obligations).  $1: output name.  Raw toolbox output.
D=/mnt/nvme/leanp1-m6; T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; cd $D
O=$D/${1:-run}.out
{ echo "M6 $(date -u +%Y-%m-%dT%H:%M:%SZ) $(md5sum LeanP1ProofM6.tla)"
  systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --method smt --stretch 12 --threads 3 -k LeanP1ProofM6.tla 2>&1
  echo "rc=$?"; } > $O 2>&1
