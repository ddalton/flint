#!/bin/bash
# M6, final: two CONTROLS, each a shipped rule the proof cites dropped from
# Shipped (LeanP1ProofM0.tla) -- ConsumeHonorsScope (ScopeRespected must fail at
# the take) and UnlinkChecksBytes (AgentWorkKept must fail at the unlink) --
# then the RECORD run, --cleanfp.  tlapm checks only LeanP1ProofM6.tla.
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; D=/mnt/nvme/leanp1-m6
for c in ConsumeHonorsScope UnlinkChecksBytes; do
  C=/mnt/nvme/leanp1-m6-ctl-$c; rm -rf $C; cp -a $D $C; cd $C
  sed -i "s#^  /\\\\ ConsumeHonorsScope /\\\\ RescopeUnciteFirst#  /\\\\ ConsumeHonorsScope /\\\\ RescopeUnciteFirst#" LeanP1ProofM0.tla
  python3 - $c <<'PY'
import sys
c=sys.argv[1]; s=open('LeanP1ProofM0.tla').read()
a='  /\\ ConsumeHonorsScope /\\ RescopeUnciteFirst /\\ RescopeKeepsDirty /\\ WidenKeepsLocal\n  /\\ UnlinkChecksBytes /\\ ConsumeKeepsLeft'
assert s.count(a)==1
b=a.replace('/\\ '+c+' ','')
assert b!=a
open('LeanP1ProofM0.tla','w').write(s.replace(a,b))
PY
  { echo "CONTROL $c dropped from Shipped $(date -u +%Y-%m-%dT%H:%M:%SZ) $(md5sum LeanP1ProofM0.tla LeanP1ProofM6.tla | tr '\n' ' ')"
    grep -n -A6 "^Shipped ==" LeanP1ProofM0.tla
    systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --method smt --stretch 12 --threads 3 -k LeanP1ProofM6.tla 2>&1
    echo "rc=$?"; } > $D/ctl-$c.out 2>&1
done
cd $D
{ echo "RECORD M6 $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  echo "md5: $(md5sum LeanP1Anc.tla LeanP1ProofM5D.tla LeanP1ProofM6.tla | tr '\n' ' ')"
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1ProofM6.tla (systemd-run MemoryMax=24G)"
  /usr/bin/time -v systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1ProofM6.tla 2>&1
  echo "rc=$?"; } > $D/record.out 2>&1
echo DONE > $D/final.done
