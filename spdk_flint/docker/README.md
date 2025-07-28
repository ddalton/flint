# SPDK Flint Docker Images

This directory contains specialized Docker images for different deployment patterns of SPDK Flint, optimized for Kubernetes environments.

## 🏗️ Image Architecture

We use a **layered approach** with one base image and four specialized images:

```
flint-base:latest                   (Base runtime + unified binary)
├── spdk-flint:csi-node            (DaemonSet - privileged)
├── spdk-flint:csi-controller      (Deployment - lightweight)
├── spdk-flint:dashboard-backend   (Service - web API)
└── spdk-flint:node-agent         (DaemonSet - disk management)
```

## 📦 Image Descriptions

### 1. **Base Image** (`flint-base:latest`)
- **Infrastructure Only**: SPDK libraries (with UBLK support), gRPC, spdlog, Crow
- **No Business Logic**: Contains no application code
- **UBLK Ready**: Built with UBLK support for volume exposure
- Build tools and development environment ready
- Foundation for all specialized images
- ~600MB (pure infrastructure)

### 2. **CSI Node** (`spdk-flint:csi-node`) 
- **Deployment:** DaemonSet (one per node)
- **Privileges:** Requires privileged access
- **Purpose:** CSI node plugin for volume mounting/unmounting
- **Size:** ~900MB
- **Additional Tools:** Filesystem tools, mount utilities, NVMe CLI

### 3. **CSI Controller** (`spdk-flint:csi-controller`)
- **Deployment:** Deployment (1-3 replicas)
- **Privileges:** Standard (no host access)
- **Purpose:** CSI controller for volume lifecycle management
- **Size:** ~820MB
- **Additional Tools:** Minimal - just network utilities

### 4. **Dashboard Backend** (`spdk-flint:dashboard-backend`)
- **Deployment:** Service (1-2 replicas)
- **Privileges:** Standard
- **Purpose:** REST API for monitoring dashboard
- **Size:** ~830MB
- **Additional Tools:** JSON processing, monitoring tools

### 5. **Node Agent** (`spdk-flint:node-agent`)
- **Deployment:** DaemonSet (one per storage node)
- **Privileges:** Requires privileged access
- **Purpose:** Disk discovery and SPDK setup
- **Size:** ~950MB
- **Additional Tools:** Disk management, SPDK setup, troubleshooting

## 🚀 Building Images

### Quick Build
```bash
# Build all images
./docker/build-images.sh

# Build with custom registry
REGISTRY=my-registry.com/spdk-flint ./docker/build-images.sh

# Build and push
./docker/build-images.sh all
```

### Individual Image Build
```bash
# Build base image first
docker build -f docker/Dockerfile.base -t flint-base:latest .

# Build specialized images
docker build -f docker/Dockerfile.csi-controller \
  --build-arg BASE_IMAGE=flint-base:latest \
  -t spdk-flint:csi-controller .
```

### Advanced Build Options
```bash
# Build with specific SPDK version
BUILD_ARGS="--build-arg SPDK_VERSION=v25.05.x" ./docker/build-images.sh

# Build debug versions
BUILD_ARGS="--build-arg CMAKE_BUILD_TYPE=Debug" ./docker/build-images.sh
```

## 🎯 Deployment Patterns

### CSI Controller (Deployment)
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: spdk-csi-controller
spec:
  replicas: 2
  selector:
    matchLabels:
      app: spdk-csi-controller
  template:
    metadata:
      labels:
        app: spdk-csi-controller
    spec:
      containers:
      - name: csi-controller
        image: spdk-flint:csi-controller
        env:
        - name: CSI_MODE
          value: "controller"
        - name: LOG_LEVEL
          value: "info"
        ports:
        - containerPort: 9809
          name: health
        livenessProbe:
          httpGet:
            path: /healthz
            port: 9809
          initialDelaySeconds: 10
          periodSeconds: 30
        resources:
          requests:
            memory: "256Mi"
            cpu: "100m"
          limits:
            memory: "512Mi"
            cpu: "500m"
```

### CSI Node (DaemonSet)
```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: spdk-csi-node
spec:
  selector:
    matchLabels:
      app: spdk-csi-node
  template:
    metadata:
      labels:
        app: spdk-csi-node
    spec:
      hostNetwork: true
      hostPID: true
      containers:
      - name: csi-node
        image: spdk-flint:csi-node
        securityContext:
          privileged: true
        env:
        - name: CSI_MODE
          value: "csi-driver"
        - name: NODE_ID
          valueFrom:
            fieldRef:
              fieldPath: spec.nodeName
        volumeMounts:
        - name: kubelet-dir
          mountPath: /var/lib/kubelet
          mountPropagation: Bidirectional
        - name: device-dir
          mountPath: /dev
        - name: sys-dir
          mountPath: /sys
        - name: csi-socket
          mountPath: /csi
        resources:
          requests:
            memory: "512Mi"
            cpu: "200m"
          limits:
            memory: "1Gi"
            cpu: "1000m"
      volumes:
      - name: kubelet-dir
        hostPath:
          path: /var/lib/kubelet
      - name: device-dir
        hostPath:
          path: /dev
      - name: sys-dir
        hostPath:
          path: /sys
      - name: csi-socket
        hostPath:
          path: /var/lib/kubelet/plugins/spdk.csi
          type: DirectoryOrCreate
```

### Dashboard Backend (Service)
```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: spdk-dashboard-backend
spec:
  replicas: 1
  selector:
    matchLabels:
      app: spdk-dashboard-backend
  template:
    metadata:
      labels:
        app: spdk-dashboard-backend
    spec:
      containers:
      - name: dashboard-backend
        image: spdk-flint:dashboard-backend
        env:
        - name: CSI_MODE
          value: "dashboard-backend"
        - name: DASHBOARD_PORT
          value: "8080"
        ports:
        - containerPort: 8080
          name: api
        livenessProbe:
          httpGet:
            path: /health
            port: 8080
        resources:
          requests:
            memory: "128Mi"
            cpu: "50m"
          limits:
            memory: "256Mi"
            cpu: "200m"

---
apiVersion: v1
kind: Service
metadata:
  name: spdk-dashboard-backend
spec:
  selector:
    app: spdk-dashboard-backend
  ports:
  - port: 8080
    targetPort: 8080
    name: api
  type: ClusterIP
```

### Node Agent (DaemonSet)
```yaml
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: spdk-node-agent
spec:
  selector:
    matchLabels:
      app: spdk-node-agent
  template:
    metadata:
      labels:
        app: spdk-node-agent
    spec:
      hostNetwork: true
      hostPID: true
      containers:
      - name: node-agent
        image: spdk-flint:node-agent
        securityContext:
          privileged: true
        env:
        - name: CSI_MODE
          value: "node-agent"
        - name: NODE_ID
          valueFrom:
            fieldRef:
              fieldPath: spec.nodeName
        - name: DISCOVERY_INTERVAL
          value: "300"
        volumeMounts:
        - name: device-dir
          mountPath: /dev
        - name: sys-dir
          mountPath: /sys
        - name: proc-dir
          mountPath: /proc
        - name: backup-dir
          mountPath: /var/lib/spdk-csi
        ports:
        - containerPort: 8090
          name: api
        resources:
          requests:
            memory: "256Mi"
            cpu: "100m"
          limits:
            memory: "512Mi"
            cpu: "500m"
      volumes:
      - name: device-dir
        hostPath:
          path: /dev
      - name: sys-dir
        hostPath:
          path: /sys
      - name: proc-dir
        hostPath:
          path: /proc
      - name: backup-dir
        hostPath:
          path: /var/lib/spdk-csi
          type: DirectoryOrCreate
      nodeSelector:
        node-type: storage  # Only deploy on storage nodes
```

## 🔧 Configuration

### Environment Variables by Image

#### Common (All Images)
- `LOG_LEVEL`: Log level (trace, debug, info, warn, error, critical)
- `TARGET_NAMESPACE`: Kubernetes namespace for custom resources
- `SPDK_RPC_URL`: SPDK RPC endpoint (for compatibility)

#### CSI Controller
- `CSI_MODE=controller`
- `HEALTH_PORT=9809`
- `NVMEOF_TRANSPORT=tcp`
- `NVMEOF_TARGET_PORT=4420`

#### CSI Node  
- `CSI_MODE=csi-driver`
- `CSI_ENDPOINT=unix:///csi/csi.sock`
- `NODE_ID`: Kubernetes node name

#### Dashboard Backend
- `CSI_MODE=dashboard-backend`
- `DASHBOARD_PORT=8080`

#### Node Agent
- `CSI_MODE=node-agent`
- `NODE_AGENT_PORT=8090`
- `DISCOVERY_INTERVAL=300`
- `AUTO_INITIALIZE_BLOBSTORE=true`
- `BACKUP_PATH=/var/lib/spdk-csi/backups`

## 🏥 Health Checks

Each image includes specialized health checks:

- **CSI Controller:** HTTP health endpoint + process check
- **CSI Node:** CSI socket + process check
- **Dashboard:** API endpoint + Kubernetes connectivity
- **Node Agent:** API endpoint + SPDK validation

## 📊 Resource Requirements

| Image | Memory Request | Memory Limit | CPU Request | CPU Limit |
|-------|---------------|--------------|-------------|-----------|
| CSI Controller | 256Mi | 512Mi | 100m | 500m |
| CSI Node | 512Mi | 1Gi | 200m | 1000m |
| Dashboard | 128Mi | 256Mi | 50m | 200m |
| Node Agent | 256Mi | 512Mi | 100m | 500m |

## 🔐 Security Considerations

### Privileged Images (DaemonSets)
- **CSI Node**: Requires privileged access for mounting
- **Node Agent**: Requires privileged access for disk management

### Non-Privileged Images (Deployments)
- **CSI Controller**: Runs as non-root user
- **Dashboard Backend**: Runs as non-root user

## 🛠️ UBLK Runtime Requirements

All images are built with **UBLK support** for volume exposure. At runtime, ensure:

```bash
# Check UBLK support
/usr/local/bin/check-ublk

# Enable UBLK driver (if needed)
sudo modprobe ublk_drv

# Verify UBLK is available
lsmod | grep ublk
```

**Note**: UBLK kernel driver must be available on host nodes for volume operations.

### RBAC Requirements
```yaml
apiVersion: v1
kind: ServiceAccount
metadata:
  name: spdk-csi-controller

---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: spdk-csi-controller
rules:
- apiGroups: ["storage.spdk.io"]
  resources: ["spdkvolumes", "spdknodes"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
- apiGroups: [""]
  resources: ["nodes", "persistentvolumes", "persistentvolumeclaims"]
  verbs: ["get", "list", "watch"]
```

## 🚀 Quick Start

1. **Build all images:**
   ```bash
   ./docker/build-images.sh
   ```

2. **Deploy CSI Controller:**
   ```bash
   kubectl apply -f examples/csi-controller-deployment.yaml
   ```

3. **Deploy CSI Node (on storage nodes):**
   ```bash
   kubectl apply -f examples/csi-node-daemonset.yaml
   ```

4. **Deploy Dashboard:**
   ```bash
   kubectl apply -f examples/dashboard-deployment.yaml
   ```

5. **Deploy Node Agent (on storage nodes):**
   ```bash
   kubectl apply -f examples/node-agent-daemonset.yaml
   ```

## 🔍 Troubleshooting

### View logs:
```bash
# CSI Controller
kubectl logs deployment/spdk-csi-controller

# CSI Node (specific node)
kubectl logs daemonset/spdk-csi-node -l app=spdk-csi-node

# Dashboard
kubectl logs deployment/spdk-dashboard-backend

# Node Agent (specific node)
kubectl logs daemonset/spdk-node-agent -l app=spdk-node-agent
```

### Test endpoints:
```bash
# Dashboard API
kubectl port-forward service/spdk-dashboard-backend 8080:8080
curl http://localhost:8080/health

# Node Agent API
kubectl port-forward pods/spdk-node-agent-<node> 8090:8090
curl http://localhost:8090/health
```

## 📁 Files

- `Dockerfile.base` - Base image with unified binary
- `Dockerfile.csi-controller` - CSI Controller (Deployment)
- `Dockerfile.csi-node` - CSI Node Plugin (DaemonSet)
- `Dockerfile.dashboard-backend` - Dashboard API (Service)
- `Dockerfile.node-agent` - Node Agent (DaemonSet)
- `build-images.sh` - Build script for all images
- `README.md` - This documentation 