#!/bin/bash
# M5 part D: check LeanP1ProofM5D.tla (EXTENDS LeanP1ProofM5C; tlapm checks only
# this module's obligations).  $1: output name.  Raw toolbox output.
D=/mnt/nvme/leanp1-m5d; T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; cd $D
O=$D/${1:-run}.out
{ echo "M5D $(date -u +%Y-%m-%dT%H:%M:%SZ) $(md5sum LeanP1ProofM5D.tla)"
  systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --method smt --stretch 12 --threads 3 -k LeanP1ProofM5D.tla 2>&1
  echo "rc=$?"; } > $O 2>&1
