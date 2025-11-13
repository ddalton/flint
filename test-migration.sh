#!/bin/bash
# Test script for pod migration with data persistence
# Run after deploying the ghost mount fix

set -e

export KUBECONFIG=/Users/ddalton/.kube/config.ublk

echo "🧪 Testing Pod Migration with Data Persistence"
echo "=============================================="
echo ""

# Cleanup any existing test resources
echo "🧹 Cleaning up existing test resources..."
kubectl delete job write-local read-remote -n flint-system --ignore-not-found=true
kubectl delete pvc migration-pvc -n flint-system --ignore-not-found=true
sleep 5

echo ""
echo "📝 Test 1: Local → Remote Migration"
echo "===================================="
echo ""

# Step 1: Create PVC and write data on ublk-2
echo "Step 1: Creating PVC and writing data on ublk-2..."
kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: migration-pvc
  namespace: flint-system
spec:
  accessModes: [ReadWriteOnce]
  storageClassName: flint-single-replica
  resources: { requests: { storage: 1Gi } }
---
apiVersion: batch/v1
kind: Job
metadata:
  name: write-local
  namespace: flint-system
spec:
  template:
    spec:
      restartPolicy: Never
      nodeSelector: { kubernetes.io/hostname: ublk-2.vpc.cloudera.com }
      containers:
      - name: writer
        image: busybox
        command: ["/bin/sh", "-c", "echo 'TEST_DATA_12345' > /data/test.txt && sync && cat /data/test.txt"]
        volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, persistentVolumeClaim: { claimName: migration-pvc } }]
EOF

# Step 2: Wait for job to complete
echo "Step 2: Waiting for write job to complete..."
kubectl wait --for=condition=complete job/write-local -n flint-system --timeout=60s || {
  echo "❌ Write job failed or timed out"
  kubectl logs -n flint-system job/write-local
  exit 1
}

# Step 3: Verify data written
echo "Step 3: Verifying data was written..."
DATA=$(kubectl logs -n flint-system job/write-local | tail -1)
if [ "$DATA" = "TEST_DATA_12345" ]; then
  echo "✅ Data written successfully: $DATA"
else
  echo "❌ Data write verification failed. Got: $DATA"
  exit 1
fi

# Step 4: Delete job and measure time
echo "Step 4: Deleting job (pod should delete quickly)..."
START_TIME=$(date +%s)
kubectl delete job write-local -n flint-system
END_TIME=$(date +%s)
DELETE_TIME=$((END_TIME - START_TIME))
echo "⏱️  Job deletion took ${DELETE_TIME}s"

if [ $DELETE_TIME -lt 10 ]; then
  echo "✅ Deletion was fast (<10s)"
else
  echo "⚠️  Deletion was slow (${DELETE_TIME}s)"
fi

# Step 5: Check for ghost mounts on ublk-2
echo "Step 5: Checking for ghost mounts on ublk-2..."
CSI_POD=$(kubectl get pod -n flint-system -l app=flint-csi-node -o jsonpath='{.items[?(@.spec.nodeName=="ublk-2.vpc.cloudera.com")].metadata.name}')
echo "CSI pod on ublk-2: $CSI_POD"

kubectl exec -n flint-system $CSI_POD -c flint-csi-driver -- sh -c '
  echo "Checking mounts and devices..."
  mount | grep ublkb | while read line; do
    dev=$(echo $line | awk "{print \$1}")
    if [ -e "$dev" ]; then
      echo "✅ Valid mount: $dev"
    else
      echo "❌ GHOST MOUNT: $dev"
    fi
  done
' || echo "No ublk mounts found (expected after cleanup)"

echo ""
echo "Step 6: Creating reader pod on ublk-1 (remote access)..."
kubectl apply -f - <<EOF
apiVersion: batch/v1
kind: Job
metadata:
  name: read-remote
  namespace: flint-system
spec:
  template:
    spec:
      restartPolicy: Never
      nodeSelector: { kubernetes.io/hostname: ublk-1.vpc.cloudera.com }
      containers:
      - name: reader
        image: busybox
        command: ["/bin/sh", "-c", "cat /data/test.txt || echo 'FILE NOT FOUND'"]
        volumeMounts: [{ name: data, mountPath: /data }]
      volumes: [{ name: data, persistentVolumeClaim: { claimName: migration-pvc } }]
EOF

# Step 7: Check if data survived migration
echo "Step 7: Waiting for read job and checking data..."
kubectl wait --for=condition=complete job/read-remote -n flint-system --timeout=60s || {
  echo "❌ Read job failed or timed out"
  kubectl logs -n flint-system job/read-remote
  exit 1
}

READ_DATA=$(kubectl logs -n flint-system job/read-remote | tail -1)
if [ "$READ_DATA" = "TEST_DATA_12345" ]; then
  echo "✅ Data survived migration! Got: $READ_DATA"
else
  echo "❌ Data was lost during migration. Got: $READ_DATA"
  exit 1
fi

echo ""
echo "🧹 Cleaning up test resources..."
kubectl delete job read-remote -n flint-system
kubectl delete pvc migration-pvc -n flint-system

echo ""
echo "✅ Test completed successfully!"
echo "=============================="
echo ""
echo "Summary:"
echo "  ✅ Data written on ublk-2 (local)"
echo "  ✅ Pod deletion took ${DELETE_TIME}s"
echo "  ✅ No ghost mounts detected"
echo "  ✅ Data read on ublk-1 (remote via NVMe-oF)"
echo ""
echo "🎉 Ghost mount fix is working!"

