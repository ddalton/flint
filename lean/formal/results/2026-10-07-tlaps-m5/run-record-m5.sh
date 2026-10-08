#!/bin/bash
# M5 part A's record run: the whole module, fingerprints erased, nothing
# else heavy on the box; then the two controls (make-controls.py) with the
# record's fingerprints.
#   systemd-run --user --unit m5record -p MemoryMax=28G run-record-m5.sh
P=/mnt/nvme/leanp1-m5-2026-10-07/proof
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m5-2026-10-07/controls
cd $P
rm -rf .tlacache
{ echo "RECORD RUN M0+M1+M2+M3+M4+M5A $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla (systemd-run MemoryMax=28G)"
  nproc; uptime; free -g | sed -n 2p; } > out/runrecord.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> out/runrecord.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/runrecord.out
for c in ctl-blm ctl-cor; do
  cd $C/$c || continue
  rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
  cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
  { echo "CONTROL $c $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > $c.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $c.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $c.out
done
echo CTLDONE > $C/ctl.done
