#!/bin/bash
cd /mnt/nvme/leanp1-v4/m0
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" systemd-run --user --scope -p MemoryMax=20G nice -n 5 $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 2 -k LeanP1Proof.tla > m0.out 2>&1
echo M0DONE >> m0.out
