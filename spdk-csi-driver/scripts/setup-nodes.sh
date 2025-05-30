#!/bin/bash
# Setup script for Kubernetes nodes

set -e

echo "Setting up SPDK CSI node..."

# Configure hugepages
echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
echo 'vm.nr_hugepages=1024' >> /etc/sysctl.conf

# Load kernel modules
modprobe vfio-pci
modprobe uio_pci_generic
echo 'vfio-pci' >> /etc/modules
echo 'uio_pci_generic' >> /etc/modules

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
