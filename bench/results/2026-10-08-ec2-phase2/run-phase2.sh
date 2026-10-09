#!/usr/bin/env bash
# Phase 2 as run (approved 2026-10-08): the three competitors on bench3, plan
# defaults, Mayastor again at the end as the return-to-first drift check.
# Flint is NOT in this run: F74 is unfixed, its Phase 1 numbers stand, and it
# re-runs on the same instance type after the fix.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
OUT=results/2026-10-08-ec2-phase2
# CEILINGS=0 from the 17:5x relaunch on: ceilings/ finished at 17:49 (all
# three disks, then a verified wipe) before Mayastor's install failed.
DRIVERS="mayastor longhorn rook-ceph mayastor" CEILINGS=0 OUT="$OUT" ./phase2.sh > "$OUT/phase2.log" 2>&1
echo "phase2 EXIT=$? $(date -u +%FT%TZ)" >> "$OUT/status"
