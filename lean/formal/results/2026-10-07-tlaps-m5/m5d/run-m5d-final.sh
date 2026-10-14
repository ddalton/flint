#!/bin/bash
# M5 part D, final: (1) the CONTROL MA1 on LeanP1Anc.tla (the gateway's delete
# leaves no tombstone and no udel entry) -- the proof must fail, at GDelete_D;
# (2) the RECORD run, --cleanfp, of LeanP1ProofM5D.tla.  tlapm checks only
# this module's obligations.
T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm; D=/mnt/nvme/leanp1-m5d; C=/mnt/nvme/leanp1-m5d-ctl
rm -rf $C; cp -a $D $C; cd $C
python3 - <<'PY'
s=open('LeanP1Anc.tla').read()
old="  /\\ tomb' = [tomb EXCEPT ![p] = doc[p]]\n  /\\ udel' = udel \\cup {<<p, doc[p]>>}\n  /\\ seq' = seq + 1 /\\ reqs' = reqs + 1\n"
new="  /\\ UNCHANGED <<tomb, udel>>\n  /\\ seq' = seq + 1 /\\ reqs' = reqs + 1\n"
assert s.count(old)==1; open('LeanP1Anc.tla','w').write(s.replace(old,new))
PY
{ echo "CONTROL MA1 $(date -u +%Y-%m-%dT%H:%M:%SZ) $(md5sum LeanP1Anc.tla LeanP1ProofM5D.tla | tr '\n' ' ')"
  systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --method smt --stretch 12 --threads 3 -k LeanP1ProofM5D.tla 2>&1
  echo "rc=$?"; } > $D/ctl-MA1.out 2>&1
cd $D
{ echo "RECORD M5D $(date -u +%Y-%m-%dT%H:%M:%SZ) tlapm $($T --version 2>&1 | head -1)"
  echo "md5: $(md5sum LeanP1Anc.tla LeanP1ProofM5C.tla LeanP1ProofM5D.tla | tr '\n' ' ')"
  echo "cmd: tlapm --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1ProofM5D.tla (systemd-run MemoryMax=24G)"
  /usr/bin/time -v systemd-run --user --scope -p MemoryMax=24G nice -n 5 $T --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads 3 -k LeanP1ProofM5D.tla 2>&1
  echo "rc=$?"; } > $D/record.out 2>&1
echo DONE > $D/final.done
