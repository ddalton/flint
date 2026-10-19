#!/usr/bin/env bash
# The export + gVisor drill on a real cluster, end to end (no teardown:
# the cluster and bucket belong to whoever provisioned them).
#
#   KUBECONFIG=... BUCKET=... KEYFILE=<iam access-key json> TAG=drill-<sha7> \
#     [REGION=us-west-1] [PREFIX=exdrill] ./forge/e2e/export/run.sh
#
# Images: dilipdalton/flint-forge-{operator,syncer,git}:$TAG and the agent
# image dilipdalton/flint-forge-agent-drill:$TAG (export/Dockerfile.agent).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd); root=$(cd "$here/../../.." && pwd)
: "${BUCKET:?}"; : "${KEYFILE:?}"; : "${TAG:?}"
export BUCKET TAG PREFIX=${PREFIX:-exdrill}; REGION=${REGION:-us-west-1}
AK=$(jq -r .AccessKey.AccessKeyId "$KEYFILE"); SK=$(jq -r .AccessKey.SecretAccessKey "$KEYFILE")
kubectl create namespace forge-system --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl create namespace agents --dry-run=client -o yaml | kubectl apply -f - >/dev/null
kubectl -n agents create secret generic forge-creds --from-literal=AWS_ACCESS_KEY_ID="$AK" \
  --from-literal=AWS_SECRET_ACCESS_KEY="$SK" --from-literal=AWS_REGION="$REGION" \
  --dry-run=client -o yaml | kubectl apply -f - >/dev/null
helm upgrade --install flint-forge "$root/flint-forge-chart" -n forge-system \
  --set image.tag="$TAG" \
  --set server.gitImage="dilipdalton/flint-forge-git:$TAG" \
  --set server.syncerImage="dilipdalton/flint-forge-syncer:$TAG" \
  --set door.deploy=true --set door.namespace=forge-system \
  --wait --timeout 6m || { echo "helm install failed"; exit 1; }
envsubst '$BUCKET $PREFIX $TAG' < "$here/rig.yaml.tpl" | kubectl apply -f -
for r in exl exp; do kubectl -n agents wait --for=condition=Available "deploy/forge-$r" --timeout=6m; done
kubectl -n agents wait --for=condition=Ready pod/s3cli pod/leanreader --timeout=5m
kubectl -n agents get flintrepo -o wide
"$here/gvisor-setup.sh"
echo; "$here/../f9-export.sh"; F9=$?
echo; "$here/../g-agent-gvisor.sh"; GV=$?
echo "f9-export rc=$F9  g-agent-gvisor rc=$GV"
