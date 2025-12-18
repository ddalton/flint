#!/bin/bash
# Deploy pNFS and standalone NFS on Kubernetes cluster
# Usage: ./deploy-all.sh

set -e

KUBECONFIG=${KUBECONFIG:-/Users/ddalton/.kube/config.cdrv}
export KUBECONFIG

echo "======================================"
echo "Deploying pNFS Test Environment"
echo "======================================"
echo ""
echo "Using KUBECONFIG: $KUBECONFIG"
echo ""

# Check cluster is accessible
echo "Checking cluster connectivity..."
kubectl cluster-info
echo ""

# Create namespace
echo "Creating namespace..."
kubectl apply -f pnfs-namespace.yaml

# Deploy MDS
echo ""
echo "Deploying MDS..."
kubectl apply -f pnfs-mds-config.yaml
kubectl apply -f pnfs-mds-deployment.yaml

# Wait for MDS to be ready
echo "Waiting for MDS to be ready..."
kubectl wait --for=condition=ready pod -l app=pnfs-mds -n pnfs-test --timeout=120s

# Deploy DS
echo ""
echo "Deploying Data Servers (DS)..."
kubectl apply -f pnfs-ds-config.yaml
kubectl apply -f pnfs-ds-daemonset.yaml

# Wait a bit for DS to start
sleep 10

# Deploy standalone NFS for comparison
echo ""
echo "Deploying Standalone NFS for comparison..."
kubectl apply -f standalone-nfs-deployment.yaml

# Wait for standalone NFS to be ready
echo "Waiting for standalone NFS to be ready..."
kubectl wait --for=condition=ready pod -l app=standalone-nfs -n pnfs-test --timeout=120s

# Deploy client pod
echo ""
echo "Deploying test client..."
kubectl apply -f pnfs-client-pod.yaml

echo "Waiting for client pod to be ready..."
kubectl wait --for=condition=ready pod pnfs-client -n pnfs-test --timeout=60s

echo ""
echo "======================================"
echo "Deployment Status"
echo "======================================"
echo ""
kubectl get all -n pnfs-test
echo ""

echo "======================================"
echo "MDS Logs (checking for DS registration)"
echo "======================================"
kubectl logs -l app=pnfs-mds -n pnfs-test --tail=50
echo ""

echo "======================================"
echo "DS Logs"
echo "======================================"
kubectl logs -l app=pnfs-ds -n pnfs-test --tail=20
echo ""

echo "======================================"
echo "Deployment Complete!"
echo "======================================"
echo ""
echo "Next steps:"
echo "1. Install NFS tools in client:"
echo "   kubectl exec -n pnfs-test pnfs-client -- bash -c 'apt-get update && apt-get install -y nfs-common fio'"
echo ""
echo "2. Mount pNFS:"
echo "   kubectl exec -n pnfs-test pnfs-client -- bash -c 'mkdir -p /mnt/pnfs && mount -t nfs -o vers=4.1 pnfs-mds:/ /mnt/pnfs'"
echo ""
echo "3. Mount standalone NFS:"
echo "   kubectl exec -n pnfs-test pnfs-client -- bash -c 'mkdir -p /mnt/standalone && mount -t nfs -o vers=4.1 standalone-nfs:/ /mnt/standalone'"
echo ""
echo "4. Run tests with: ./run-performance-tests.sh"
echo ""

