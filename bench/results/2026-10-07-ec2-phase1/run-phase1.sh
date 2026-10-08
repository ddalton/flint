#!/usr/bin/env bash
# Phase 1 as run: Flint 1.58.0 r1 then r3, plan defaults.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench2.yaml
for sc in r1 r3; do
  SC=flint-bench-$sc DRIVER=flint-$sc CPU_PATTERNS='spdk=spdk_tgt,agent=csi-driver,nfs=flint-nfs-server' OUT=results/2026-10-07-ec2-phase1/flint-$sc     SIZE=100Gi FILE_SIZE=90G RUNTIME=120 RAMP=30 REPS=3 ./run-fio.sh > results/2026-10-07-ec2-phase1/flint-$sc.log 2>&1
  echo "flint-$sc EXIT=$?" >> results/2026-10-07-ec2-phase1/phase1.status
done
echo ALL-DONE >> results/2026-10-07-ec2-phase1/phase1.status
