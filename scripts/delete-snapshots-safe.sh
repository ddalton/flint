#!/bin/bash
# Safe snapshot deletion - deletes in reverse chronological order

set -e

KUBECONFIG=${KUBECONFIG:-/Users/ddalton/.kube/config.flnt}

echo "⚠️  This will delete ALL snapshots for the volume"
echo "   Snapshots will be deleted in reverse order (safe)"
echo ""
echo "Snapshots to delete:"
kubectl get volumesnapshot -n default | grep snapshot-

echo ""
read -p "Continue? (yes/no): " confirm

if [ "$confirm" != "yes" ]; then
    echo "Cancelled"
    exit 0
fi

echo ""
echo "🗑️  Step 1: Delete snapshot-after-200mb (newest first)"
kubectl delete volumesnapshot snapshot-after-200mb -n default

echo "⏳ Waiting 5 seconds..."
sleep 5

echo ""
echo "🗑️  Step 2: Delete snapshot-baseline (oldest last)"
kubectl delete volumesnapshot snapshot-baseline -n default

echo ""
echo "✅ All snapshots deleted successfully"
echo ""
echo "Checking remaining resources..."
kubectl get volumesnapshot -n default
kubectl get volumesnapshotcontent | grep pvc-b0eeb198 || echo "No snapshot content remaining"
