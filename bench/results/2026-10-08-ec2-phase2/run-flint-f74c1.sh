#!/usr/bin/env bash
# run-flint-f74c1.sh -- appended 2026-10-08 (user: "push the test image and
# add the run"). F74 change 1 measured alone: chart 1.58.0 with the csi-driver
# image swapped for the TEST build dilipdalton/flint-driver:1.58.0-f74c1
# (commit b34a954a: 128 KiB lvstore clusters, md ratio 13; digest
# sha256:ab94d006cf6f3a05a4f632847a9fef00f04dcaba278ec6ca7e364025ff7d24fc),
# shipped 300 s epoch interval, r3 only. Its control is flint-r3-epoch300
# from run-flint-epochs.sh on this same cluster.
#
# Waits for run-flint-epochs.sh to finish ALL-DONE. Before the matrix, reads
# every node's lvstore cluster size and refuses to measure unless all are
# 131072 (a 1 MiB lvstore would silently measure the old code).
set -u
cd "/Users/ddalton/github/flint/bench"
export KUBECONFIG=$HOME/.kube/bench3.yaml
R=results/2026-10-08-ec2-phase2
D=$R/flint-r3-f74c1
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) flint-f74c1: $*" >> "$R/status"; }

until grep -q -E "flint-epochs: (ALL-DONE|FAILED|phase2 did not finish)" <(sed -n '/relaunch after mayastor crd fix/,$p' "$R/status"); do sleep 60; done
if ! sed -n '/relaunch after mayastor crd fix/,$p' "$R/status" | grep -q "flint-epochs: ALL-DONE"; then
  status "epoch runs did not finish ALL-DONE -- f74c1 run SKIPPED"; exit 1
fi

. drivers/flint/env.sh
status "install flint 1.58.0 + test image 1.58.0-f74c1 (epoch 300)"
if ! VERSION=1.58.0 HELM_EXTRA="--set crds.installSnapshotCRDs=false --set images.flintCsiDriver.tag=1.58.0-f74c1 --set replication.orchestrators.epochIntervalSecs=300" \
     drivers/flint/ec2-up.sh > "$R/install-flint-f74c1.log" 2>&1; then
  status "FAILED install (f74c1)"; exit 1
fi
kubectl -n flint-system get pods -o jsonpath='{range .items[*]}{.metadata.name}{"\t"}{range .spec.containers[*]}{.image}{" "}{end}{"\n"}{end}' > "$R/flint-f74c1-images.txt"
: > "$R/flint-f74c1-lvstores.txt"
for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
  kubectl -n flint-system exec "$p" -c spdk-tgt -- python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock bdev_lvol_get_lvstores \
    >> "$R/flint-f74c1-lvstores.txt" 2>&1
done
n_new=$(grep -c '"cluster_size": 131072' "$R/flint-f74c1-lvstores.txt")
n_all=$(grep -c '"cluster_size"' "$R/flint-f74c1-lvstores.txt")
if [ "$n_all" -lt 3 ] || [ "$n_new" != "$n_all" ]; then
  status "FAILED: lvstores not all 128 KiB ($n_new of $n_all) -- NOT measuring"; exit 1
fi
status "flint r3 matrix (f74c1, $n_new lvstores at 128 KiB)"
SC=$SC_R3 DRIVER=flint-r3-f74c1 CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="$REACTOR_TICKS" OUT=$D \
  SIZE=100Gi FILE_SIZE=90G RUNTIME=120 RAMP=30 REPS=3 ./run-fio.sh > "$D.log" 2>&1
rc=$?
kubectl -n flint-system logs deploy/flint-csi-controller --all-containers --timestamps > "$R/flint-f74c1-controller.log" 2>&1
grep -E "\[EPOCH\]" "$R/flint-f74c1-controller.log" > "$R/flint-f74c1-epoch-events.txt"
[ $rc = 0 ] || { status "FAILED r3 matrix (f74c1, rc=$rc)"; exit 1; }
status "ALL-DONE"
