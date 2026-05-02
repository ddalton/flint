#!/usr/bin/env bash
# kind-down.sh — tear down the regression-suite kind cluster.
set -euo pipefail

CLUSTER_NAME="${CLUSTER_NAME:-flint-regression}"

if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER_NAME"; then
    echo "kind cluster '$CLUSTER_NAME' does not exist, nothing to do"
    exit 0
fi

echo "▶ deleting kind cluster '$CLUSTER_NAME'"
kind delete cluster --name "$CLUSTER_NAME"

# Remove the auto-generated values file from kind-up.sh.
rm -f "$(cd "$(dirname "$0")" && pwd)/kind-values.yaml"

echo "✓ done"
