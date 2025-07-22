#!/bin/bash
# Setup script for Kubernetes nodes

set -e

echo "Setting up SPDK CSI node..."

# Configure hugepages
echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
echo 'vm.nr_hugepages=1024' >> /etc/sysctl.conf

# Load kernel modules (skip if already loaded or not available)
modprobe vfio-pci 2>/dev/null || echo "vfio-pci already loaded or not available"
modprobe uio_pci_generic 2>/dev/null || echo "uio_pci_generic not available (normal on AWS/cloud kernels)"

# Add modules to autoload (only if they exist)
if lsmod | grep -q vfio_pci; then
    echo 'vfio-pci' >> /etc/modules 2>/dev/null || true
fi
if modinfo uio_pci_generic >/dev/null 2>&1; then
    echo 'uio_pci_generic' >> /etc/modules 2>/dev/null || true
fi

# Install nvme-cli for NVMe-oF operations
apt-get update
apt-get install -y nvme-cli util-linux

# Create directories
mkdir -p /var/lib/csi/sockets/pluginproxy
mkdir -p /var/lib/kubelet/plugins/csi.spdk.io

# Setup SPDK environment
mkdir -p /etc/spdk
cat > /etc/spdk/target.conf << EOF
[Global]
ReactorMask 0x3
LogFacility local7

[Rpc]
Enable Yes
Listen 0.0.0.0:5260

[Nvmf]
TransportId trtype:tcp adrfam:ipv4 traddr:0.0.0.0 trsvcid:4420
EOF

echo "Node setup complete!"
