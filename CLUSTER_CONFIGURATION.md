# Cluster Configuration Guide

This guide helps you configure the SPDK CSI driver for different Kubernetes cluster types.

## 🎯 **Topology Configuration**

The CSI driver needs to know whether it can use standard Kubernetes topology labels or should use custom labels to avoid RBAC conflicts.

### **Configuration Option**

Set `driver.useHostnameTopology` in your `values.yaml` or Helm command:

```yaml
driver:
  useHostnameTopology: true   # For self-managed clusters
  # OR
  useHostnameTopology: false  # For managed clusters (default)
```

## 🏗️ **Self-Managed Clusters** 

**Use**: `useHostnameTopology: true`

**Examples**:
- `kubeadm` clusters
- Bare metal Kubernetes
- Custom installations with full RBAC control

**Helm Install**:
```bash
helm install flint-csi ./flint-csi-driver-chart \
  --set driver.useHostnameTopology=true \
  --set images.repository=your-registry.com/flint \
  --namespace flint-system \
  --create-namespace
```

**Benefits**: Uses standard `topology.kubernetes.io/hostname` labels for compatibility

## 🌐 **Managed Clusters**

**Use**: `useHostnameTopology: false` (default)

**Examples**:
- **Rancher**-managed clusters
- **Amazon EKS**
- **Google GKE**  
- **Azure AKS**
- **DigitalOcean Kubernetes**
- **Red Hat OpenShift**

**Helm Install**:
```bash
helm install flint-csi ./flint-csi-driver-chart \
  --set driver.useHostnameTopology=false \
  --set images.repository=your-registry.com/flint \
  --namespace flint-system \
  --create-namespace
```

**Benefits**: Uses custom `flint.csi.storage.io/node` labels to avoid RBAC conflicts

## 🔧 **How It Works**

### **When `useHostnameTopology: true`**
- Uses `topology.kubernetes.io/hostname` for volume placement
- Compatible with standard Kubernetes topology features
- Requires RBAC permissions to modify node labels

### **When `useHostnameTopology: false`**
- Uses `flint.csi.storage.io/node` for volume placement  
- Avoids protected topology labels
- Works in restrictive managed environments

## ⚙️ **Environment Variable**

The setting is passed to containers as `USE_HOSTNAME_TOPOLOGY`:

```yaml
env:
  - name: USE_HOSTNAME_TOPOLOGY
    value: "{{ .Values.driver.useHostnameTopology }}"
```

## 🚨 **Troubleshooting**

### **Error**: `nodes "node-name" is forbidden: is not allowed to modify labels: topology.kubernetes.io/hostname`

**Solution**: Set `useHostnameTopology: false`

```bash
helm upgrade flint-csi ./flint-csi-driver-chart \
  --set driver.useHostnameTopology=false \
  --reuse-values
```

### **Verify Configuration**

Check the environment variable in running pods:

```bash
kubectl exec -n flint-system deployment/flint-csi-controller -c flint-csi-controller -- env | grep HOSTNAME_TOPOLOGY
kubectl exec -n flint-system daemonset/flint-csi-node -c flint-csi-driver -- env | grep HOSTNAME_TOPOLOGY
```

## 🎯 **Recommendations**

| Cluster Type | Setting | Reason |
|--------------|---------|---------|
| **Rancher** | `false` | Protects topology labels |
| **EKS** | `false` | AWS manages node labels |
| **GKE** | `false` | Google manages node labels |
| **AKS** | `false` | Azure manages node labels |
| **kubeadm** | `true` | Full control over labels |
| **Self-built** | `true` | Custom RBAC possible |

## ✅ **Current Setup**

For your **Rancher-managed cluster**, the current configuration is correct:

```yaml
driver:
  useHostnameTopology: false  # ✅ Safe for Rancher
```

This avoids the RBAC issues you were experiencing! 🎉 