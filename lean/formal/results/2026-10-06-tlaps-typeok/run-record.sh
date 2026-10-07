#!/bin/bash
# The record run: clean fingerprints, the configuration run 12 passed with,
# then the paused tie (pid 43284) is resumed.
P=/mnt/nvme/leanp1-proof-2026-10-06
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
cd $P
n=$1
date -u +%H:%M:%SZ > out/run$n.start
{ echo "RECORD RUN $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla (systemd-run MemoryMax=24G)"
  nproc; free -g | head -2; } > out/run$n.out
systemd-run --user --scope -q -p MemoryMax=24G /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> out/run$n.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/run$n.out
kill -CONT 43284 && echo "TIE RESUMED $(date -u +%H:%M:%SZ)" >> out/run$n.out
