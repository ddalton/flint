#!/usr/bin/env bash
# Stand the forge rig up on a real cluster.
#
#   BUCKET=... KEYFILE=... ./forge/e2e/deploy.sh
#
# Idempotent: re-running upgrades the chart and re-applies the rig.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

: "${BUCKET:?set BUCKET to the drill bucket}"
: "${KEYFILE:?set KEYFILE to the IAM access-key JSON}"
PREFIX=${PREFIX:-drill}
REGION=${REGION:-us-west-1}
TAG=${TAG:-1.46.0-forge.2}
NS_SYS=${NS_SYS:-forge-system}
NS_AGENTS=${NS_AGENTS:-agents}

# THE DOOR'S OPTIONAL ROUTE TABLES AND VERIFIERS, off unless asked for.
#
# Both are deliberately opt-in here for the same reason the chart makes
# them opt-in: `--repo-files` is a second door onto tenant data, and
# `--jwt-issuer` is a second authority. A drill that needs one says so.
#
#   REPO_FILES=1                                   F13's second route table
#   JWT_ISSUER=... JWT_PUBKEY=<file> [JWT_AUDIENCE=...] [JWT_MAX_LIFETIME=...]
#                                                  F15's external issuer
REPO_FILES_SET=""
[ "${REPO_FILES:-0}" = "1" ] && REPO_FILES_SET="--set door.repoFiles.enabled=true"

JWT_SET=""
if [ -n "${JWT_ISSUER:-}" ]; then
    : "${JWT_PUBKEY:?JWT_ISSUER needs JWT_PUBKEY — the door refuses to start with an issuer and no key source}"
    JWT_SET="--set door.jwt.issuer=$JWT_ISSUER --set door.jwt.publicKeySecret.name=forge-jwt-key"
    JWT_SET="$JWT_SET --set door.jwt.maxLifetimeSecs=${JWT_MAX_LIFETIME:-3600}"
    [ -n "${JWT_AUDIENCE:-}" ] && JWT_SET="$JWT_SET --set door.jwt.audience=$JWT_AUDIENCE"
fi

AK=$(jq -r .AccessKey.AccessKeyId "$KEYFILE")
SK=$(jq -r .AccessKey.SecretAccessKey "$KEYFILE")

kubectl create namespace "$NS_SYS"    --dry-run=client -o yaml | kubectl apply -f -
kubectl create namespace "$NS_AGENTS" --dry-run=client -o yaml | kubectl apply -f -

# The syncer reads these through `envFrom`, so the KEYS ARE THE ENV VAR
# NAMES and must be AWS_* verbatim — a renamed key is a credential the
# SDK never looks for, and the failure is a timeout, not a 403.
# The door's issuer key, in the door's namespace. AFTER the namespaces
# exist: this is an object, not a flag, and computing the flags early
# does not mean creating it early.
if [ -n "${JWT_ISSUER:-}" ]; then
    kubectl -n "$NS_SYS" create secret generic forge-jwt-key \
        --from-file=public.pem="$JWT_PUBKEY" \
        --dry-run=client -o yaml | kubectl apply -f -
fi

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
    $REPO_FILES_SET $JWT_SET \
    --wait --timeout 5m

BUCKET="$BUCKET" PREFIX="$PREFIX" \
    envsubst '$BUCKET $PREFIX' < "$here/rig.yaml.tpl" | kubectl apply -f -

echo "── waiting for the repository to serve ──"
kubectl -n "$NS_AGENTS" wait --for=condition=Available deploy/forge-proj --timeout=5m 2>/dev/null || true
kubectl -n "$NS_AGENTS" get flintrepo proj -o wide || true
