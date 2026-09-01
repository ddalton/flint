#!/bin/bash
set -eu
export KUBECONFIG=/tmp/trove-aws-kc-runbw
AB=/Users/ddalton/github/flint/tests/k8s/oci-ab
SCRATCH=/private/tmp/claude-503/-Users-ddalton-github-flint/6cf3cf28-c178-4262-afb6-7c1596140f69/scratchpad/oci-ab

echo "== DS placement =="
kubectl -n flint-system get pods -o wide --no-headers | grep pnfs-ds | awk '{print $1, $7}'
# client = the worker with no DS pod
ds_nodes=$(kubectl -n flint-system get pods -o wide --no-headers | grep pnfs-ds | awk '{print $7}' | sort -u)
client=""
for n in $(kubectl get nodes --no-headers | grep -v control-plane | awk '{print $1}'); do
  echo "$ds_nodes" | grep -q "^$n$" || client=$n
done
[ -n "$client" ] || { echo "no DS-free worker; picking runbw-aws-4"; client=runbw-aws-4; }
echo "client node: $client"
kubectl label node "$client" oci-ab/role=client --overwrite

echo "== WireGuard off (recorded arm property) =="
kubectl -n kube-system patch cm cilium-config --type merge -p '{"data":{"enable-wireguard":"false"}}'
kubectl -n kube-system rollout restart ds/cilium
kubectl -n kube-system rollout status ds/cilium --timeout 5m

echo "== S3 secret + registries =="
. "$SCRATCH/s3.env"
kubectl create secret generic oci-ab-s3 \
  --from-literal=REGISTRY_STORAGE_S3_BUCKET="$BUCKET" \
  --from-literal=REGISTRY_STORAGE_S3_REGION=us-west-1 \
  --from-literal=REGISTRY_STORAGE_S3_ACCESSKEY="$S3_KEY_ID" \
  --from-literal=REGISTRY_STORAGE_S3_SECRETKEY="$S3_SECRET" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl apply -f "$AB/registries.yaml"
kubectl rollout status deploy/registry-flint --timeout 8m
kubectl rollout status deploy/registry-s3 --timeout 5m
kubectl get svc registry-flint registry-s3 -o jsonpath='{range .items[*]}{.metadata.name}{" "}{.spec.clusterIP}{"\n"}{end}'
echo "setup done"
