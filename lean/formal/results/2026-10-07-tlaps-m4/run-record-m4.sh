#!/bin/bash
# M4's record run, after M3's chain (and its ctl-cag rerun) has ended: the
# whole module, fingerprints erased, the TLC unit (m3big) frozen; then the
# RetireAge control with the record's fingerprints; then the unit thawed.
P=/mnt/nvme/leanp1-m4-2026-10-07/proof
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
C=/mnt/nvme/leanp1-m4-2026-10-07/controls
until test -f /mnt/nvme/leanp1-m3-2026-10-07/controls/cag.done; do sleep 30; done
cd $P
systemctl --user freeze m3big 2>/dev/null; sleep 2
{ echo "RECORD RUN M0+M1+M2+M3+M4 $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  md5sum LeanP1Proof.tla LeanP1Anc.tla
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla (systemd-run MemoryMax=28G; TLC unit frozen)"
  nproc; uptime; } > out/runrecord.out
/usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> out/runrecord.out 2>&1
echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> out/runrecord.out
for c in ctl-rage4; do
  cd $C/$c || continue
  rm -rf .tlacache; mkdir -p .tlacache/LeanP1Proof.tlaps
  cp $P/.tlacache/LeanP1Proof.tlaps/fingerprints .tlacache/LeanP1Proof.tlaps/
  { echo "CONTROL $c $(date -u +%Y-%m-%dT%H:%M:%SZ)"; md5sum LeanP1Anc.tla LeanP1Proof.tla; } > $c.out
  /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $T --toolbox 0 0 --method smt --stretch 12 --threads 4 -k LeanP1Proof.tla >> $c.out 2>&1
  echo "PROOFDONE $(date -u +%H:%M:%SZ)" >> $c.out
done
systemctl --user thaw m3big 2>/dev/null
echo CTLDONE > $C/ctl.done
