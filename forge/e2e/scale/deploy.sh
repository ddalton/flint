#!/usr/bin/env bash
# Stand the SCALE rig up on a real cluster: forge/e2e/deploy.sh's shape
# with this directory's rig — the chart with the door, the credentials
# secret, two repositories and one agent.
#
#   BUCKET=... KEYFILE=... TAG=drill-<sha7> ./forge/e2e/scale/deploy.sh
#
# TAG is REQUIRED and has no default on purpose. The drill verifies
# claims about the current tree, so the images must be built from it
# (forge/e2e/build-forge-images.sh with ARCH=amd64 PUSH=1 TAG=...); a
# default that named a release would run a release, and this repository
# has already shipped two images whose tag said one thing and whose
# content was another.
#
# Idempotent: re-running upgrades the chart and re-applies the rig. The
# PREFIX defaults to a fresh timestamp so a re-run never reads a
# previous run's snapshot; it is printed at the end because run-scale.sh
# needs it.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)

: "${BUCKET:?set BUCKET to the drill bucket}"
: "${KEYFILE:?set KEYFILE to the IAM access-key JSON}"
: "${TAG:?set TAG to the images built from this tree (build-forge-images.sh PUSH=1)}"
PREFIX=${PREFIX:-scale-$(date +%Y%m%d%H%M)}
REGION=${REGION:-us-west-1}
NS_SYS=${NS_SYS:-forge-system}
NS_AGENTS=${NS_AGENTS:-agents}

AK=$(jq -r .AccessKey.AccessKeyId "$KEYFILE")
SK=$(jq -r .AccessKey.SecretAccessKey "$KEYFILE")

kubectl create namespace "$NS_SYS"    --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace "$NS_AGENTS" --dry-run=client -o yaml | kubectl apply -f -

# envFrom: the KEYS ARE THE ENV VAR NAMES, AWS_* verbatim.
kubectl -n "$NS_AGENTS" create secret generic forge-creds \
    --from-literal=AWS_ACCESS_KEY_ID="$AK" \
    --from-literal=AWS_SECRET_ACCESS_KEY="$SK" \
    --from-literal=AWS_REGION="$REGION" \
    --dry-run=client -o yaml | kubectl apply -f -

helm upgrade --install flint-forge "$root/flint-forge-chart" \
    -n "$NS_SYS" \
    --set image.tag="$TAG" \
    --set server.gitImage="dilipdalton/flint-forge-git:$TAG" \
    --set server.syncerImage="dilipdalton/flint-forge-syncer:$TAG" \
    --set door.deploy=true \
    --set door.namespace="$NS_SYS" \
    --wait --timeout 5m

BUCKET="$BUCKET" PREFIX="$PREFIX" TAG="$TAG" \
    envsubst '$BUCKET $PREFIX $TAG' < "$here/rig.yaml.tpl" | kubectl apply -f -

echo "── waiting for both repositories to serve ──"
for r in big small; do
    kubectl -n "$NS_AGENTS" wait --for=condition=Available "deploy/forge-$r" --timeout=5m 2>/dev/null || true
done
kubectl -n "$NS_AGENTS" wait --for=condition=Ready pod/agent1 --timeout=3m 2>/dev/null || true
kubectl -n "$NS_AGENTS" get flintrepo big small -o wide || true
echo
echo "deployed: images :$TAG, bucket $BUCKET, prefix $PREFIX"
echo "next:     BUCKET=$BUCKET PREFIX=$PREFIX KEYFILE=$KEYFILE $here/run-scale.sh"
