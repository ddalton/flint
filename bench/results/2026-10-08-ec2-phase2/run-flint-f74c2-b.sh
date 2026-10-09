#!/usr/bin/env bash
# run-flint-f74c2-b.sh -- appended 2026-10-08 (user: "yes, push the image and
# add the run"). F74 changes 1 + 2 together: chart 1.58.0 with the csi-driver
# TEST image 1.58.0-f74c1 (128 KiB lvstore clusters, md ratio 13) AND the
# spdk-tgt TEST image dilipdalton/spdk-tgt:1.7.0-f74c2 (commit 95db6c03:
# blob-parallel-cluster-alloc.patch, up to 32 cluster copies in flight per
# channel; digest sha256:c761a2acec86ad98405d89b426bebf7270c00a4fd09eb5f0d430b8292af109c2),
# shipped 300 s epochs, r3 only. Controls on this cluster: flint-r3-f74c1
# (change 1 alone) and flint-r3-epoch300 (neither).
#
# Waits for run-flint-f74c1-b.sh to finish ALL-DONE. That arm leaves Flint
# installed, so this one uninstalls and wipes first. Before the matrix it
# refuses to measure unless every node runs the f74c2 spdk-tgt image and
# every lvstore is 128 KiB.
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
R=results/2026-10-08-ec2-phase2
D=$R/flint-r3-f74c2
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) flint-f74c2: $*" >> "$R/status"; }

until grep -q -E "flint-f74c1: (ALL-DONE|FAILED|epoch runs did not finish)" <(sed -n '/relaunch after longhorn disk-pressure fix/,$p' "$R/status"); do sleep 60; done
if ! sed -n '/relaunch after longhorn disk-pressure fix/,$p' "$R/status" | grep -q "flint-f74c1: ALL-DONE"; then
  status "f74c1 run did not finish ALL-DONE -- f74c2 run SKIPPED"; exit 1
fi

. drivers/flint/env.sh
status "uninstall flint (f74c1)"
drivers/flint/uninstall.sh > "$R/uninstall-flint-f74c1.log" 2>&1 &&
  CONFIRM_WIPE=yes drivers/wipe-disks.sh > "$R/wipe-flint-f74c1.log" 2>&1 ||
  { status "FAILED uninstall/wipe (f74c1)"; exit 1; }
status "install flint 1.58.0 + driver 1.58.0-f74c1 + spdk-tgt 1.7.0-f74c2 (epoch 300)"
if ! VERSION=1.58.0 HELM_EXTRA="--set crds.installSnapshotCRDs=false --set images.flintCsiDriver.tag=1.58.0-f74c1 --set images.spdkTarget.tag=1.7.0-f74c2 --set replication.orchestrators.epochIntervalSecs=300" \
     drivers/flint/ec2-up.sh > "$R/install-flint-f74c2.log" 2>&1; then
  status "FAILED install (f74c2)"; exit 1
fi
kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{range .spec.initContainers[*]}{.image}{" "}{end}{range .spec.containers[*]}{.image}{" "}{end}{"\n"}{end}' > "$R/flint-f74c2-images.txt"
n_nodes=$(kubectl -n flint-system get pods -l app=flint-csi-node -o name | wc -l | tr -d ' ')
n_tgt=$(grep flint-csi-node "$R/flint-f74c2-images.txt" | grep -c "spdk-tgt:1.7.0-f74c2")
if [ "$n_nodes" -lt 3 ] || [ "$n_tgt" != "$n_nodes" ]; then
  status "FAILED: spdk-tgt f74c2 on $n_tgt of $n_nodes node pods -- NOT measuring"; exit 1
fi
: > "$R/flint-f74c2-lvstores.txt"
for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
  kubectl -n flint-system exec "$p" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock bdev_lvol_get_lvstores \
    >> "$R/flint-f74c2-lvstores.txt" 2>&1
done
n_new=$(grep -c '"cluster_size": 131072' "$R/flint-f74c2-lvstores.txt")
n_all=$(grep -c '"cluster_size"' "$R/flint-f74c2-lvstores.txt")
if [ "$n_all" -lt 3 ] || [ "$n_new" != "$n_all" ]; then
  status "FAILED: lvstores not all 128 KiB ($n_new of $n_all) -- NOT measuring"; exit 1
fi
status "flint r3 matrix (f74c2, $n_tgt spdk-tgt f74c2, $n_new lvstores at 128 KiB)"
SC=$SC_R3 DRIVER=flint-r3-f74c2 CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="$REACTOR_TICKS" OUT=$D \
  SIZE=100Gi FILE_SIZE=90G RUNTIME=120 RAMP=30 REPS=3 ./run-fio.sh > "$D.log" 2>&1
rc=$?
kubectl -n flint-system logs deploy/flint-csi-controller --all-containers --timestamps > "$R/flint-f74c2-controller.log" 2>&1
grep -E "\[EPOCH\]" "$R/flint-f74c2-controller.log" > "$R/flint-f74c2-epoch-events.txt"
[ $rc = 0 ] || { status "FAILED r3 matrix (f74c2, rc=$rc)"; exit 1; }
status "ALL-DONE"
