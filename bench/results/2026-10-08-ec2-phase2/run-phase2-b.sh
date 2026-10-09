#!/usr/bin/env bash
# run-phase2-b.sh -- relaunch of run-phase2.sh after Longhorn's install died
# 2026-10-08 20:40Z: its image pull on top of Mayastor's put all three 8 GiB
# roots into DiskPressure, the kubelet evicted the samplers, and hostexec
# failed. Fixed by growing every root to 32 GiB (EBS modify + growpart +
# xfs_growfs), containerd discard_unpacked_layers=true, critical-priority
# samplers, and prune_images between drivers. Mayastor's runs stand; this
# continues with the rest of the order. DONE_LIST makes the repeat label
# itself mayastor-a-again2.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
OUT=results/2026-10-08-ec2-phase2
DRIVERS="longhorn rook-ceph mayastor" DONE_LIST="mayastor" CEILINGS=0 OUT="$OUT" ./phase2.sh > "$OUT/phase2-b.log" 2>&1
echo "phase2 EXIT=$? $(date -u +%FT%TZ)" >> "$OUT/status"
