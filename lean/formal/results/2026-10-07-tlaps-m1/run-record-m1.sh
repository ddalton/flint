#!/bin/bash
# M1's record run (17): fingerprints erased, the TLC checkers paused, then the
# three controls (each with the record's fingerprints copied in), then the
# checkers resumed.
P=/mnt/nvme/leanp1-proof-2026-10-06
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m1-2026-10-07/controls
cd $P
pkill -STOP -f "[t]lcgen-mcleanp1m1" ; sleep 2
pkill -9 -f "[z]3 -smt2"
n=17
date -u +%H:%M:%SZ > out/run$n.start
{ echo "RECORD RUN M0+M1 $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla (systemd-run MemoryMax=24G; TLC checkers SIGSTOPped)"
  nproc; uptime; } > out/run$n.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> out/run$n.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/run$n.out
for c in ctl-claim ctl-dwp ctl-ruf; do
  cd $C/$c || continue
  rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
  cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
  { echo "CONTROL $c $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > $c.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $c.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $c.out
done
pkill -CONT -f "[t]lcgen-mcleanp1m1"
echo CTLDONE > $C/ctl.done
