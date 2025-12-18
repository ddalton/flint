#!/bin/bash
# Build pNFS images with device ID fix and debug logging
# Usage: ./build-and-deploy-fixes.sh

set -e

KUBECONFIG=${KUBECONFIG:-/Users/ddalton/.kube/config.cdrv}
export KUBECONFIG

BUILD_HOST="root@cdrv-1.vpc.cloudera.com"
BUILD_DIR="/root/flint/spdk-csi-driver"
PNFS_IMAGE="docker-sandbox.infra.cloudera.com/ddalton/pnfs:debug-v1"
STANDALONE_IMAGE="docker-sandbox.infra.cloudera.com/ddalton/flint-driver:latest"

echo "======================================"
echo "Building pNFS Images with Fixes"
echo "======================================"
echo ""
echo "Fixes included:"
echo "  ✅ Device ID environment variable substitution"
echo "  ✅ Enhanced debug logging for:"
echo "     - DS registration with actual device IDs"
echo "     - EXCHANGE_ID flag modifications"
echo "     - Layout generation and segmentation"
echo "     - LAYOUTGET requests"
echo ""

# Step 1: Copy modified source files to build server
echo "Step 1: Copying modified source files to build server..."
echo ""

scp /Users/ddalton/projects/rust/flint/spdk-csi-driver/src/pnfs/config.rs \
    $BUILD_HOST:$BUILD_DIR/src/pnfs/config.rs

scp /Users/ddalton/projects/rust/flint/spdk-csi-driver/src/pnfs/mds/device.rs \
    $BUILD_HOST:$BUILD_DIR/src/pnfs/mds/device.rs

scp /Users/ddalton/projects/rust/flint/spdk-csi-driver/src/pnfs/mds/operations/mod.rs \
    $BUILD_HOST:$BUILD_DIR/src/pnfs/mds/operations/mod.rs

scp /Users/ddalton/projects/rust/flint/spdk-csi-driver/src/nfs_ds_main.rs \
    $BUILD_HOST:$BUILD_DIR/src/nfs_ds_main.rs

echo "✅ Source files copied"
echo ""

# Step 2: Build pNFS image on remote server
echo "Step 2: Building pNFS image (MDS + DS) on $BUILD_HOST..."
echo ""

ssh $BUILD_HOST "cd $BUILD_DIR && \
    echo '🔨 Building pNFS image with debug logging...' && \
    docker buildx build --platform linux/amd64 \
      -f docker/Dockerfile.pnfs \
      -t $PNFS_IMAGE \
      --push . && \
    echo '✅ pNFS image built and pushed: $PNFS_IMAGE'"

echo ""
echo "✅ pNFS image built successfully"
echo ""

# Step 3: Delete existing namespace and redeploy
echo "Step 3: Redeploying with new images..."
echo ""

echo "Deleting existing namespace..."
kubectl delete namespace pnfs-test --wait=true --timeout=60s || echo "Namespace already deleted"

echo ""
echo "Waiting 5 seconds for cleanup..."
sleep 5

# Step 4: Deploy with new image
echo ""
echo "Deploying with new pNFS image..."

# Update deployment files to use new image tag
cat > /tmp/pnfs-mds-deployment-debug.yaml <<EOF
apiVersion: apps/v1
kind: Deployment
metadata:
  name: pnfs-mds
  namespace: pnfs-test
  labels:
    app: pnfs-mds
spec:
  replicas: 1
  selector:
    matchLabels:
      app: pnfs-mds
  template:
    metadata:
      labels:
        app: pnfs-mds
    spec:
      containers:
      - name: mds
        image: $PNFS_IMAGE
        imagePullPolicy: Always
        command: ["/usr/local/bin/flint-pnfs-mds"]
        args:
          - "--config"
          - "/etc/flint/pnfs.yaml"
          - "--verbose"
        ports:
        - containerPort: 2049
          name: nfs
          protocol: TCP
        - containerPort: 50051
          name: grpc
          protocol: TCP
        volumeMounts:
        - name: config
          mountPath: /etc/flint
        - name: data
          mountPath: /data
      volumes:
      - name: config
        configMap:
          name: pnfs-mds-config
      - name: data
        emptyDir: {}
---
apiVersion: v1
kind: Service
metadata:
  name: pnfs-mds
  namespace: pnfs-test
spec:
  selector:
    app: pnfs-mds
  ports:
  - name: nfs
    port: 2049
    targetPort: 2049
    protocol: TCP
  - name: grpc
    port: 50051
    targetPort: 50051
    protocol: TCP
  type: ClusterIP
EOF

cat > /tmp/pnfs-ds-daemonset-debug.yaml <<EOF
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: pnfs-ds
  namespace: pnfs-test
  labels:
    app: pnfs-ds
spec:
  selector:
    matchLabels:
      app: pnfs-ds
  template:
    metadata:
      labels:
        app: pnfs-ds
    spec:
      containers:
      - name: ds
        image: $PNFS_IMAGE
        imagePullPolicy: Always
        command: ["/usr/local/bin/flint-pnfs-ds"]
        args:
          - "--config"
          - "/etc/flint/pnfs.yaml"
          - "--verbose"
        env:
        - name: NODE_NAME
          valueFrom:
            fieldRef:
              fieldPath: spec.nodeName
        ports:
        - containerPort: 2049
          name: nfs
          protocol: TCP
        volumeMounts:
        - name: config
          mountPath: /etc/flint
        - name: data
          mountPath: /mnt/pnfs-data
        securityContext:
          privileged: true
      volumes:
      - name: config
        configMap:
          name: pnfs-ds-config
      - name: data
        emptyDir: {}
EOF

# Deploy everything
kubectl apply -f pnfs-namespace.yaml
kubectl apply -f pnfs-mds-config.yaml
kubectl apply -f /tmp/pnfs-mds-deployment-debug.yaml
kubectl apply -f pnfs-ds-config.yaml
kubectl apply -f /tmp/pnfs-ds-daemonset-debug.yaml
kubectl apply -f standalone-nfs-deployment.yaml
kubectl apply -f pnfs-client-pod.yaml

echo ""
echo "Waiting for pods to be ready..."
kubectl wait --for=condition=ready pod -l app=pnfs-mds -n pnfs-test --timeout=120s
kubectl wait --for=condition=ready pod -l app=standalone-nfs -n pnfs-test --timeout=120s
kubectl wait --for=condition=ready pod pnfs-client -n pnfs-test --timeout=60s

# Wait a bit for DS to register
echo ""
echo "Waiting for DS pods to register..."
sleep 15

echo ""
echo "======================================"
echo "Deployment Status"
echo "======================================"
kubectl get pods -n pnfs-test -o wide
echo ""

echo "======================================"
echo "Verification: Device Registration"
echo "======================================"
echo ""
echo "Checking MDS logs for device registration..."
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -A2 "Device registry\|Registering new device" || echo "No device registration logs yet"
echo ""

echo "Checking MDS status report..."
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -A4 "MDS Status Report" | tail -10
echo ""

echo "======================================"
echo "Verification: pNFS Flags"
echo "======================================"
echo ""
echo "Checking for EXCHANGE_ID flag modifications..."
kubectl logs -l app=pnfs-mds -n pnfs-test | grep -A3 "EXCHANGE_ID" || echo "No EXCHANGE_ID logs yet (will appear when client mounts)"
echo ""

echo "======================================"
echo "Verification: DS Device IDs"
echo "======================================"
echo ""
echo "Checking DS logs for device IDs..."
kubectl logs -l app=pnfs-ds -n pnfs-test | grep "Device ID\|NODE_NAME" | head -10
echo ""

echo "======================================"
echo "Build and Deploy Complete!"
echo "======================================"
echo ""
echo "Next steps:"
echo "1. Run performance tests:"
echo "   cd /Users/ddalton/projects/rust/flint/deployments"
echo "   KUBECONFIG=/Users/ddalton/.kube/config.cdrv ./run-performance-tests.sh"
echo ""
echo "2. Check detailed logs:"
echo "   kubectl logs -l app=pnfs-mds -n pnfs-test | grep '🎯\\|📊\\|✅'"
echo "   kubectl logs -l app=pnfs-ds -n pnfs-test | grep 'Device ID'"
echo ""
echo "3. Monitor status:"
echo "   watch 'kubectl get pods -n pnfs-test && echo && kubectl logs -l app=pnfs-mds -n pnfs-test | grep \"Status Report\" -A4 | tail -10'"
echo ""

