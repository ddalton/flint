#!/bin/bash
# Setup script for Kubernetes nodes

set -e

echo "Setting up SPDK CSI node..."

# Function to detect CPU vendor
detect_cpu_vendor() {
    if grep -q "GenuineIntel" /proc/cpuinfo; then
        echo "intel"
    elif grep -q "AuthenticAMD" /proc/cpuinfo; then
        echo "amd"
    else
        echo "unknown"
    fi
}

# Function to check if IOMMU is enabled
check_iommu_enabled() {
    if [ -d "/sys/kernel/iommu_groups" ] && [ "$(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)" -gt 0 ]; then
        echo "IOMMU is enabled ($(ls /sys/kernel/iommu_groups/ | wc -l) groups found)"
        return 0
    else
        echo "IOMMU is not enabled"
        return 1
    fi
}

# Function to configure IOMMU in GRUB
configure_iommu() {
    local cpu_vendor=$(detect_cpu_vendor)
    local iommu_params=""
    
    case $cpu_vendor in
        "intel")
            iommu_params="intel_iommu=on iommu=pt"
            echo "Detected Intel CPU - will configure with: $iommu_params"
            ;;
        "amd")
            iommu_params="amd_iommu=on iommu=pt"
            echo "Detected AMD CPU - will configure with: $iommu_params"
            ;;
        *)
            echo "Warning: Unknown CPU vendor, using generic IOMMU settings"
            iommu_params="iommu=pt"
            ;;
    esac
    
    # Check if IOMMU parameters are already in GRUB config
    if grep -q "$iommu_params" /etc/default/grub; then
        echo "IOMMU parameters already present in GRUB config"
        return 0
    fi
    
    echo "Adding IOMMU parameters to GRUB configuration..."
    
    # Backup original GRUB config
    cp /etc/default/grub /etc/default/grub.backup.$(date +%Y%m%d_%H%M%S)
    
    # Add IOMMU parameters to GRUB_CMDLINE_LINUX
    if grep -q 'GRUB_CMDLINE_LINUX=' /etc/default/grub; then
        # Update existing line
        sed -i "s/GRUB_CMDLINE_LINUX=\"/&$iommu_params /" /etc/default/grub
    else
        # Add new line
        echo "GRUB_CMDLINE_LINUX=\"$iommu_params\"" >> /etc/default/grub
    fi
    
    # Update GRUB
    if command -v update-grub >/dev/null 2>&1; then
        update-grub
    elif command -v grub2-mkconfig >/dev/null 2>&1; then
        grub2-mkconfig -o /boot/grub2/grub.cfg
    else
        echo "Warning: Could not find GRUB update command. Please update GRUB manually."
        return 1
    fi
    
    echo "GRUB configuration updated successfully"
    return 2  # Indicates reboot needed
}

# Check and setup IOMMU
echo "Checking IOMMU configuration..."
if ! check_iommu_enabled; then
    echo "IOMMU is required for SPDK vfio-pci driver. Configuring IOMMU..."
    configure_iommu
    iommu_result=$?
    
    if [ $iommu_result -eq 2 ]; then
        echo ""
        echo "================================================"
        echo "IMPORTANT: IOMMU has been configured in GRUB"
        echo "A REBOOT IS REQUIRED before SPDK will work!"
        echo ""
        echo "After reboot, verify IOMMU with:"
        echo "  ls /sys/kernel/iommu_groups/ | wc -l"
        echo "  (should show > 0 groups)"
        echo "================================================"
        echo ""
        echo "Continuing with rest of setup..."
    fi
fi

# Configure hugepages
echo "Configuring hugepages..."
echo 1024 > /sys/kernel/mm/hugepages/hugepages-2048kB/nr_hugepages
echo 'vm.nr_hugepages=1024' >> /etc/sysctl.conf

# Load kernel modules (skip if already loaded or not available)
echo "Loading kernel modules..."
modprobe vfio-pci 2>/dev/null || echo "vfio-pci already loaded or not available"
modprobe uio_pci_generic 2>/dev/null || echo "uio_pci_generic not available (normal on AWS/cloud kernels)"

# Add modules to autoload (only if they exist)
if lsmod | grep -q vfio_pci; then
    echo 'vfio-pci' >> /etc/modules 2>/dev/null || true
fi
if modinfo uio_pci_generic >/dev/null 2>&1; then
    echo 'uio_pci_generic' >> /etc/modules 2>/dev/null || true
fi

# Install required packages
echo "Installing required packages..."
apt-get update
apt-get install -y nvme-cli util-linux

# Create directories
echo "Creating required directories..."
mkdir -p /var/lib/csi/sockets/pluginproxy
mkdir -p /var/lib/kubelet/plugins/csi.spdk.io

# Setup SPDK environment
echo "Setting up SPDK configuration..."
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

echo ""
echo "Node setup complete!"

# Final IOMMU check and recommendations
if check_iommu_enabled; then
    echo "✓ IOMMU is properly enabled"
    echo "✓ Node is ready for SPDK operations"
else
    echo ""
    echo "⚠️  WARNING: IOMMU is still not enabled!"
    echo "   This node may not work properly with SPDK."
    echo "   Please reboot if IOMMU was just configured."
    echo ""
    echo "To verify IOMMU after reboot:"
    echo "  cat /proc/cmdline | grep iommu"
    echo "  ls /sys/kernel/iommu_groups/ | wc -l"
fi

echo ""
