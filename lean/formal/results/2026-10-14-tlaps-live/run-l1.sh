#!/bin/bash
# L1: the CONTROL (GatewayIgnoresLease dropped from Shipped in M0: L1_Enabled
# must fail, a lease holder keeps GCas disabled), then the RECORD run,
# --cleanfp.  tlapm checks only LeanP1ProofL1.tla (M0's theorems are facts).
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; D=/mnt/nvme/leanp1-l1
C=$D-ctl; rm -rf $C; mkdir -p $C; cp $D/LeanP1Anc.tla $D/LeanP1ProofM0.tla $D/LeanP1ProofL1.tla $C; cd $C
python3 - <<'PY'
s=open('LeanP1ProofM0.tla').read()
a='  /\\ GatewayIgnoresLease /\\ GatewayJudgesRead'
assert s.count(a)==1
open('LeanP1ProofM0.tla','w').write(s.replace(a,'  /\\ GatewayJudgesRead'))
PY
{ echo "CONTROL GatewayIgnoresLease dropped from Shipped $(date -u +%Y-%m-%dT%H:%M:%SZ) $(md5sum *.tla | tr '\n' ' ')"
  grep -n -A6 "^Shipped ==" LeanP1ProofM0.tla
  nice -n 5 $T --toolbox 0 0 --cleanfp --stretch 12 --threads 2 -k LeanP1ProofL1.tla 2>&1
  echo "rc=$?"; } > $D/ctl-GatewayIgnoresLease.out 2>&1
cd $D
{ echo "RECORD L1 $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  echo "md5: $(md5sum LeanP1Anc.tla LeanP1ProofM0.tla LeanP1ProofL1.tla | tr '\n' ' ')"
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --stretch 12 --threads 2 -k LeanP1ProofL1.tla"
  /usr/bin/time -v nice -n 5 $T --toolbox 0 0 --cleanfp --stretch 12 --threads 2 -k LeanP1ProofL1.tla 2>&1
  echo "rc=$?"; } > $D/record.out 2>&1
echo DONE > $D/final.done
