#!/usr/bin/env bash
# run-flint-epochs.sh -- appended to Phase 2 on 2026-10-08 (user: "add the
# epoch interval run for Flint at the end"). F74 change 4 measured on its own:
# Flint 1.58.0 as released, r3 only (r1 has no epochs), with the epoch
# interval raised to 3600 s, then the shipped 300 s as a same-cluster control
# (Phase 1's 300 s numbers come from a different cluster). Nothing else
# changes: same chart, same image, same matrix as Phase 1.
#
# Waits for run-phase2.sh to finish; runs only if it ended ALL-DONE.
# Controller logs (epoch cut times: "[EPOCH] Common epoch recorded") are
# saved per arm so results can be split by where the cuts fell.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
R=results/2026-10-08-ec2-phase2
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) flint-epochs: $*" >> "$R/status"; }

until grep -q -E "ALL-DONE|phase2 EXIT=" <(sed -n '/relaunch after longhorn disk-pressure fix/,$p' "$R/status"); do sleep 60; done
if ! sed -n '/relaunch after longhorn disk-pressure fix/,$p' "$R/status" | grep -q ALL-DONE; then
  status "phase2 did not finish ALL-DONE -- Flint epoch runs SKIPPED"; exit 1
fi

. drivers/flint/env.sh
for I in 3600 300; do
  D=$R/flint-r3-epoch$I
  status "install flint 1.58.0 epochIntervalSecs=$I"
  if ! VERSION=1.58.0 HELM_EXTRA="--set crds.installSnapshotCRDs=false --set replication.orchestrators.epochIntervalSecs=$I" \
       drivers/flint/ec2-up.sh > "$R/install-flint-epoch$I.log" 2>&1; then
    status "FAILED install (epoch $I)"; exit 1
  fi
  kubectl -n flint-system get deploy -o yaml | grep -A1 FLINT_EPOCH_INTERVAL_SECS > "$R/flint-epoch$I-env.txt"
  status "flint r3 matrix (epoch $I)"
  SC=$SC_R3 DRIVER=flint-r3-epoch$I CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="$REACTOR_TICKS" OUT=$D \
    SIZE=100Gi FILE_SIZE=90G RUNTIME=120 RAMP=30 REPS=3 ./run-fio.sh > "$D.log" 2>&1
  rc=$?
  kubectl -n flint-system logs deploy/flint-csi-controller --all-containers --timestamps > "$R/flint-epoch$I-controller.log" 2>&1
  grep -E "\[EPOCH\]" "$R/flint-epoch$I-controller.log" > "$R/flint-epoch$I-epoch-events.txt"
  [ $rc = 0 ] || { status "FAILED r3 matrix (epoch $I, rc=$rc)"; exit 1; }
  status "uninstall flint (epoch $I)"
  drivers/flint/uninstall.sh > "$R/uninstall-flint-epoch$I.log" 2>&1 &&
    CONFIRM_WIPE=yes drivers/wipe-disks.sh > "$R/wipe-flint-epoch$I.log" 2>&1 ||
    { status "FAILED uninstall/wipe (epoch $I)"; exit 1; }
done
status "ALL-DONE"
