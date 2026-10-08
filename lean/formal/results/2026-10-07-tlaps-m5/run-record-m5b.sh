#!/bin/bash
# M5 part B's record run: the whole module (M0-M4 + M5 parts A and B),
# fingerprints erased, nothing else heavy on the box.  Part A's record peaked
# at 27.9 GiB under a 28G cap, so this one gets 29G (the box has 30 + 7 swap).
#   systemd-run --user --unit m5brecord -p MemoryMax=29G run-record-m5b.sh
D=/mnt/nvme/leanp1-m5-2026-10-07/recordb
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $D
rm -rf .tlacache; mkdir -p out
{ echo "RECORD RUN M0+M1+M2+M3+M4+M5A+M5B $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla (systemd-run MemoryMax=29G)"
  nproc; uptime; free -g | sed -n 2p; } > out/runrecord.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> out/runrecord.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/runrecord.out
