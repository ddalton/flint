#!/usr/bin/env bash
# ec2-up.sh -- put a RELEASED Flint on a trove EC2 cluster for the benchmark:
# helm upgrade to VERSION with ec2-values.yaml, then initialize each node's
# instance-store NVMe through the production path (the node agent's
# POST /api/disks/initialize, as tests/system/results/2026-10-02-aws-spdk
# did), then apply the bench StorageClasses.
#
# Run AFTER ceilings.sh (which writes the raw disks).
# CHART is an OCI reference; the package is pulled and its version checked.
#
#   KUBECONFIG=~/.kube/bench2.yaml VERSION=1.58.0 ./ec2-up.sh
#
# HELM_EXTRA: extra helm arguments, word-split, e.g.
#   HELM_EXTRA="--set replication.orchestrators.epochIntervalSecs=3600"
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
: "${VERSION:?e.g. 1.58.0}"
CHART="${CHART:-oci://registry-1.docker.io/dilipdalton/flint-csi-driver-chart}"
DISK_MODEL="${DISK_MODEL:-Amazon EC2 NVMe Instance Storage}"
step() { printf '\n▶ %s\n' "$*" >&2; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

step "helm upgrade flint-csi to $VERSION"
# Pull into a private dir: on the Mac, helm's default cache dirs are
# root-owned and an OCI install from them fails ("failed to download").
pull=$(mktemp -d)
helm pull "$CHART" --version "$VERSION" -d "$pull"
pkg=$(ls "$pull"/*.tgz)
got=$(helm show chart "$pkg" | awk '/^version:/{print $2; exit}')   # reads the LOCAL package
[ "$got" = "$VERSION" ] || fail "pulled chart is version '$got', wanted $VERSION"
helm upgrade --install flint-csi "$pkg" \
  --namespace flint-system --create-namespace -f "$HERE/ec2-values.yaml" ${HELM_EXTRA:-} --wait --timeout 15m
kubectl -n flint-system wait --for=condition=Ready pod -l app=flint-csi-node --timeout=600s   # OnDelete DS: no rollout status
helm -n flint-system list

step "initialize each node's instance store"
for p in $(kubectl -n flint-system get pods -l app=flint-csi-node -o name); do
  node=$(kubectl -n flint-system get "$p" -o jsonpath='{.spec.nodeName}')
  port=$(( 19081 + RANDOM % 1000 ))
  kubectl -n flint-system port-forward "$p" "$port:9081" >/dev/null 2>&1 &
  pf=$!
  for _ in $(seq 1 30); do curl -s -m 2 "http://127.0.0.1:$port/api/disks" >/dev/null && break; sleep 1; done
  disks=$(curl -s -m 30 "http://127.0.0.1:$port/api/disks")
  pci=$(python3 -c '
import json, sys
model = sys.argv[1]
d = json.loads(sys.stdin.read())
rows = d if isinstance(d, list) else d.get("disks", d.get("data", []))
# The agent may already hold the disk as a uring bdev, and then reports its
# model as "URING bdev"; so take THE non-system disk, and only if unique.
hits = [r for r in rows if model in (r.get("model") or "")] or \
       [r for r in rows if not r.get("is_system_disk")]
print(hits[0]["pci_address"] if len(hits) == 1 else "")
' "$DISK_MODEL" <<<"$disks")
  if [ -z "$pci" ]; then kill "$pf"; echo "$disks" >&2; fail "$node: not exactly one '$DISK_MODEL' (or non-system) disk in /api/disks"; fi
  echo "$node: $pci" >&2
  curl -s -m 300 -X POST -H 'Content-Type: application/json' \
    -d "{\"pci_addresses\":[\"$pci\"]}" "http://127.0.0.1:$port/api/disks/initialize" | tee /dev/stderr | grep -q '"success":true' ||
    { kill "$pf"; fail "$node: initialize failed"; }
  echo >&2
  kill "$pf"
done

kubectl apply -f "$HERE/storageclasses.yaml"
