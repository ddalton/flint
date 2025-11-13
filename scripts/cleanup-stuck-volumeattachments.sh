#!/bin/bash
# cleanup-stuck-volumeattachments.sh
# Cleans up VolumeAttachments that are stuck after pod deletion

set -euo pipefail

KUBECONFIG="${KUBECONFIG:-~/.kube/config}"

echo "🔍 Checking for orphaned VolumeAttachments..."

# Get all VolumeAttachments
vas=$(kubectl get volumeattachments -o json 2>/dev/null || echo '{"items":[]}')

# Check each VolumeAttachment
echo "$vas" | jq -r '.items[] | select(.status.attached == true) | .metadata.name' | while read -r va_name; do
    if [ -z "$va_name" ]; then
        continue
    fi
    
    # Get the PV name from the VolumeAttachment
    pv_name=$(kubectl get volumeattachment "$va_name" -o jsonpath='{.spec.source.persistentVolumeName}' 2>/dev/null || echo "")
    
    if [ -z "$pv_name" ]; then
        echo "⚠️  VolumeAttachment $va_name has no PV reference, skipping"
        continue
    fi
    
    # Check if PV exists and is being deleted or in Released state
    pv_status=$(kubectl get pv "$pv_name" -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
    pv_deletion=$(kubectl get pv "$pv_name" -o jsonpath='{.metadata.deletionTimestamp}' 2>/dev/null || echo "")
    
    # Get PVC reference
    pvc_name=$(kubectl get pv "$pv_name" -o jsonpath='{.spec.claimRef.name}' 2>/dev/null || echo "")
    pvc_namespace=$(kubectl get pv "$pv_name" -o jsonpath='{.spec.claimRef.namespace}' 2>/dev/null || echo "")
    
    # Check if PVC exists
    pvc_exists="false"
    if [ -n "$pvc_name" ] && [ -n "$pvc_namespace" ]; then
        kubectl get pvc "$pvc_name" -n "$pvc_namespace" >/dev/null 2>&1 && pvc_exists="true"
    fi
    
    # Check if any pods are using this PVC
    pods_using_pvc=0
    if [ "$pvc_exists" == "true" ]; then
        pods_using_pvc=$(kubectl get pods -n "$pvc_namespace" -o json | \
            jq -r --arg pvc "$pvc_name" '.items[] | select(.spec.volumes[]?.persistentVolumeClaim?.claimName == $pvc) | .metadata.name' | \
            wc -l | tr -d ' ')
    fi
    
    should_delete="false"
    reason=""
    
    # Determine if we should delete this VolumeAttachment
    if [ "$pv_status" == "NotFound" ]; then
        should_delete="true"
        reason="PV not found"
    elif [ -n "$pv_deletion" ]; then
        should_delete="true"
        reason="PV is being deleted"
    elif [ "$pv_status" == "Released" ] && [ "$pvc_exists" == "false" ]; then
        should_delete="true"
        reason="PV released and PVC deleted"
    elif [ "$pvc_exists" == "false" ] && [ "$pods_using_pvc" -eq 0 ]; then
        should_delete="true"
        reason="PVC deleted and no pods using it"
    elif [ "$pods_using_pvc" -eq 0 ]; then
        # PVC exists but no pods are using it - this might be intentional, so be cautious
        echo "ℹ️  VolumeAttachment $va_name: PVC exists but no pods using it (not deleting)"
    fi
    
    if [ "$should_delete" == "true" ]; then
        echo "🗑️  Deleting orphaned VolumeAttachment: $va_name (reason: $reason)"
        kubectl delete volumeattachment "$va_name"
    fi
done

echo "✅ Cleanup complete"

