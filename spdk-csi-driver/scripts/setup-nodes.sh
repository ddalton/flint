#!/bin/bash

# Enhanced SPDK CSI Node Setup Script for Bare Metal and Virtualized Environments
set -e

echo "Setting up SPDK CSI node..."

# Function to detect if we're in a virtualized environment
detect_virtualization() {
    local is_virtual=false
    
    # Check DMI product name
    if [ -f /sys/class/dmi/id/product_name ]; then
        local product=$(cat /sys/class/dmi/id/product_name | tr '[:upper:]' '[:lower:]')
        if [[ $product == *"virtualbox"* ]] || \
           [[ $product == *"vmware"* ]] || \
           [[ $product == *"qemu"* ]] || \
           [[ $product == *"kvm"* ]] || \
           [[ $product == *"xen"* ]] || \
           [[ $product == *"amazon ec2"* ]]; then
            is_virtual=true
        fi
    fi
    
    # Check for hypervisor flag in CPU
    if grep -q "hypervisor" /proc/cpuinfo 2>/dev/null; then
        is_virtual=true
    fi
    
    # Check systemd-detect-virt if available
    if command -v systemd-detect-virt >/dev/null 2>&1; then
        local virt_type=$(systemd-detect-virt 2>/dev/null || echo "none")
        if [ "$virt_type" != "none" ]; then
            is_virtual=true
        fi
    fi
    
    echo $is_virtual
}

# Function to check and install SPDK userspace drivers for bare metal
setup_bare_metal_drivers() {
    echo "=== Bare Metal SPDK Driver Setup ==="
    echo "On bare metal, IOMMU is NOT required for SPDK!"
    echo "We'll set up optimal userspace drivers..."
    
    # 1. Check for uio_pci_generic (most common)
    echo "Checking uio_pci_generic availability..."
    if modprobe --dry-run uio_pci_generic >/dev/null 2>&1; then
        echo "✅ uio_pci_generic is available (no IOMMU required)"
        modprobe uio_pci_generic
    else
        echo "❌ uio_pci_generic not available in this kernel"
    fi
    
    # 2. Check for igb_uio (better compatibility)
    echo "Checking igb_uio availability..."
    if modprobe --dry-run igb_uio >/dev/null 2>&1; then
        echo "✅ igb_uio is available (no IOMMU required)"
        modprobe igb_uio
    else
        echo "❌ igb_uio not available - can be installed from dpdk-kmods"
        echo "To install igb_uio:"
        echo "  git clone https://github.com/DPDK/dpdk-kmods.git"
        echo "  cd dpdk-kmods/linux/igb_uio && make && sudo insmod igb_uio.ko"
    fi
    
    # 3. Set up vfio with no-IOMMU mode as fallback
    echo "Setting up VFIO no-IOMMU mode as fallback..."
    if modprobe vfio-pci >/dev/null 2>&1; then
        echo "1" > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode 2>/dev/null || true
        echo "✅ VFIO no-IOMMU mode enabled"
    fi
    
    echo ""
    echo "🎯 BARE METAL RECOMMENDATIONS:"
    echo "1. Primary choice: uio_pci_generic (no IOMMU needed)"
    echo "2. Alternative: igb_uio (better device compatibility)" 
    echo "3. Fallback: vfio-pci no-IOMMU mode"
    echo "4. Flint will auto-select the best available driver"
    echo ""
}

# Function to set up IOMMU for virtualized environments
setup_virtualized_iommu() {
    echo "=== Virtualized Environment IOMMU Setup ==="
    echo "In VMs, IOMMU provides security isolation for SPDK"
    
    # Check current IOMMU configuration
    echo "Checking IOMMU configuration..."
    if [ -d "/sys/kernel/iommu_groups" ]; then
        iommu_groups=$(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)
        if [ $iommu_groups -gt 0 ]; then
            echo "✅ IOMMU is already enabled ($iommu_groups groups)"
            return 0
        fi
    fi
    
    echo "IOMMU is not enabled"
    echo "IOMMU is required for SPDK vfio-pci driver in virtualized environments."
    
    # Configure IOMMU based on CPU type
    echo "Configuring IOMMU..."
    
    # Detect CPU vendor
    if grep -q "Intel" /proc/cpuinfo; then
        IOMMU_PARAMS="intel_iommu=on iommu=pt"
        echo "Detected Intel CPU - will configure with: $IOMMU_PARAMS"
    elif grep -q "AMD" /proc/cpuinfo; then
        IOMMU_PARAMS="amd_iommu=on iommu=pt"
        echo "Detected AMD CPU - will configure with: $IOMMU_PARAMS"
    else
        IOMMU_PARAMS="iommu=pt"
        echo "Unknown CPU - will configure with: $IOMMU_PARAMS"
    fi
    
    # Update GRUB configuration
    echo "Adding IOMMU parameters to GRUB configuration..."
    if ! grep -q "$IOMMU_PARAMS" /etc/default/grub; then
        sed -i "s/GRUB_CMDLINE_LINUX_DEFAULT=\"/&$IOMMU_PARAMS /" /etc/default/grub
        update-grub
        echo "GRUB configuration updated successfully"
        echo ""
        echo "⚠️  REBOOT REQUIRED to enable IOMMU"
        echo "After reboot, verify with: ls /sys/kernel/iommu_groups/ | wc -l"
    else
        echo "IOMMU parameters already present in GRUB configuration"
    fi
}

# Main setup logic
main() {
    local is_virtual=$(detect_virtualization)
    
    echo "Environment detection:"
    if [ "$is_virtual" = "true" ]; then
        echo "🖥️  Detected: Virtualized environment"
        setup_virtualized_iommu
    else
        echo "🔧 Detected: Bare metal environment"
        setup_bare_metal_drivers
    fi
    
    # Set up hugepages (required for both environments)
    echo "=== Setting up hugepages ==="
    echo "Setting up hugepages for SPDK..."
    
    # Calculate reasonable hugepage allocation (1GB or 25% of RAM, whichever is smaller)
    total_mem_kb=$(grep MemTotal /proc/meminfo | awk '{print $2}')
    total_mem_gb=$((total_mem_kb / 1024 / 1024))
    
    if [ $total_mem_gb -gt 4 ]; then
        hugepage_gb=$(( total_mem_gb / 4 ))
        if [ $hugepage_gb -gt 1 ]; then
            hugepage_gb=1
        fi
    else
        hugepage_gb=1
    fi
    
    hugepages_2m=$((hugepage_gb * 512))  # 2MB pages
    
    echo "Allocating ${hugepage_gb}GB (${hugepages_2m} x 2MB pages) for hugepages"
    echo $hugepages_2m > /proc/sys/vm/nr_hugepages
    
    # Mount hugepages
    if ! mount | grep -q hugetlbfs; then
        mkdir -p /dev/hugepages
        mount -t hugetlbfs hugetlbfs /dev/hugepages
        echo "Hugepages mounted at /dev/hugepages"
    fi
    
    # Verify allocation
    actual_hugepages=$(cat /proc/sys/vm/nr_hugepages)
    echo "Allocated hugepages: $actual_hugepages"
    
    echo ""
    echo "🎉 SPDK CSI node setup completed!"
    echo ""
    echo "📋 Summary:"
    if [ "$is_virtual" = "true" ]; then
        echo "- Environment: Virtualized (IOMMU recommended)"
        echo "- Driver: vfio-pci (with IOMMU for security)"
    else
        echo "- Environment: Bare metal (IOMMU not required)"
        echo "- Drivers: uio_pci_generic, igb_uio, or vfio-pci no-IOMMU"
    fi
    echo "- Hugepages: ${hugepage_gb}GB allocated"
    echo "- Auto-detection: Flint will select optimal driver"
    echo ""
    echo "Next steps:"
    echo "1. Deploy Flint CSI driver to Kubernetes"
    echo "2. Flint will automatically choose the best SPDK driver"
    echo "3. No manual driver selection needed!"
}

# Run main function
main "$@"
