#!/usr/bin/env bash
# storageclass-local.sh <node> -- print the locality-pass StorageClass: the
# r1 bench class with its replica pinned to <node>'s DiskPool (needs the
# pools labelled by install.sh POOL_LABELS=1).
set -euo pipefail
cat <<YAML
apiVersion: storage.k8s.io/v1
kind: StorageClass
metadata: {name: mayastor-bench-r1-local}
provisioner: io.openebs.csi-mayastor
parameters:
  protocol: nvmf
  repl: "1"
  thin: "false"
  fsType: ext4
  poolAffinityTopologyLabel: |
    bench-pool: $1
reclaimPolicy: Delete
volumeBindingMode: WaitForFirstConsumer
YAML
