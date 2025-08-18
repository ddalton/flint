#!/bin/bash

# Enhanced SPDK CSI Node Setup Script for Bare Metal and Virtualized Environments
set -e

# Script configuration
SCRIPT_NAME=$(basename "$0")
ENABLE_IOMMU=false
SKIP_HUGEPAGES=false
SKIP_DRIVERS=false
VERBOSE=false

# Usage function
usage() {
    cat <<EOF
Usage: $SCRIPT_NAME [OPTIONS]

SPDK CSI Node Setup Script - Prepares nodes for SPDK CSI driver deployment

OPTIONS:
    -i, --enable-iommu      Enable IOMMU configuration (modifies GRUB, requires reboot)
                           WARNING: May affect network connectivity after reboot!
                           Ensure console/IPMI access before using this option.
                           
    -s, --skip-hugepages   Skip hugepages configuration
    
    -d, --skip-drivers     Skip userspace driver setup (uio_pci_generic, vfio-pci, etc.)
    
    -v, --verbose          Enable verbose output
    
    -h, --help             Show this help message

DESCRIPTION:
    This script prepares a node for SPDK CSI driver deployment by:
    
    1. USERSPACE DRIVERS (default: enabled)
       - Detects environment (bare metal vs virtualized)
       - Sets up appropriate drivers:
         * Bare metal: uio_pci_generic or igb_uio
         * Virtualized: vfio-pci (requires IOMMU)
       
    2. HUGEPAGES (default: enabled)
       - Configures 2MB hugepages for SPDK
       - Auto-scales based on system memory (1-4GB)
       - Makes configuration persistent
    
    3. IOMMU (default: DISABLED - must use -i flag)
       - Configures Intel or AMD IOMMU
       - Modifies GRUB configuration
       - REQUIRES REBOOT to take effect
       - Required for vfio-pci in virtualized environments

EXAMPLES:
    # Basic setup (drivers + hugepages, NO IOMMU changes)
    sudo $SCRIPT_NAME
    
    # Full setup including IOMMU (use with caution)
    sudo $SCRIPT_NAME --enable-iommu
    
    # Setup only hugepages
    sudo $SCRIPT_NAME --skip-drivers
    
    # Dry run to see what would be configured
    sudo $SCRIPT_NAME --verbose

SAFETY NOTES:
    ⚠️  IOMMU WARNING: Enabling IOMMU modifies kernel boot parameters and may:
       - Change network interface names (eth0 → ens3, etc.)
       - Cause network connectivity loss after reboot
       - Bind network cards to wrong drivers
       
    ✅ BEFORE enabling IOMMU, ensure you have:
       - Console access (physical, IPMI, or hypervisor console)
       - Network configuration backups
       - Tested on a non-production node first

RECOVERY:
    If you lose SSH access after IOMMU setup:
    1. Access via console/IPMI
    2. Check network interfaces: 'ip addr' or 'ifconfig -a'
    3. Manually restore network: 'dhclient <interface>' or 'ifup <interface>'
    4. To revert IOMMU: Remove 'intel_iommu=on iommu=pt' from /etc/default/grub
       Then run 'update-grub' and reboot

For more information, see: https://spdk.io/doc/getting_started.html

EOF
    exit 0
}

# Parse command line arguments
parse_args() {
    while [[ $# -gt 0 ]]; do
        case $1 in
            -i|--enable-iommu)
                ENABLE_IOMMU=true
                shift
                ;;
            -s|--skip-hugepages)
                SKIP_HUGEPAGES=true
                shift
                ;;
            -d|--skip-drivers)
                SKIP_DRIVERS=true
                shift
                ;;
            -v|--verbose)
                VERBOSE=true
                shift
                ;;
            -h|--help)
                usage
                ;;
            *)
                echo "Unknown option: $1"
                echo "Use -h or --help for usage information"
                exit 1
                ;;
        esac
    done
}

# Verbose logging function
log_verbose() {
    if [ "$VERBOSE" = true ]; then
        echo "[VERBOSE] $*"
    fi
}

# Function to detect if we're in a virtualized environment
detect_virtualization() {
    local is_virtual=false
    log_verbose "Detecting virtualization environment..."
    
    # Check DMI product name
    if [ -f /sys/class/dmi/id/product_name ]; then
        local product=$(cat /sys/class/dmi/id/product_name | tr '[:upper:]' '[:lower:]')
        if [[ $product == *"virtualbox"* ]] || \
           [[ $product == *"vmware"* ]] || \
           [[ $product == *"qemu"* ]] || \
           [[ $product == *"kvm"* ]] || \
           [[ $product == *"xen"* ]] || \
           [[ $product == *"amazon ec2"* ]] || \
           [[ $product == *"microsoft corporation"* ]]; then
            is_virtual=true
        fi
    fi
    
    # Check hypervisor presence
    if [ -d /proc/xen ] || [ -e /sys/hypervisor/type ] || [ -d /sys/bus/vmbus/devices ]; then
        is_virtual=true
    fi
    
    # Check systemd-detect-virt if available
    if command -v systemd-detect-virt >/dev/null 2>&1; then
        if systemd-detect-virt -q; then
            is_virtual=true
        fi
    fi
    
    echo $is_virtual
}

# Function to build and install igb_uio
setup_igb_uio() {
    echo "🔧 Setting up igb_uio driver for bare metal SPDK..."
    log_verbose "Building igb_uio from source..."
    
    # Install build dependencies
    if command -v apt-get >/dev/null 2>&1; then
        echo "Installing build dependencies (Debian/Ubuntu)..."
        apt-get update
        apt-get install -y build-essential linux-headers-$(uname -r) git
    elif command -v yum >/dev/null 2>&1; then
        echo "Installing build dependencies (RHEL/CentOS)..."
        yum groupinstall -y "Development Tools"
        yum install -y kernel-devel-$(uname -r) git
    elif command -v dnf >/dev/null 2>&1; then
        echo "Installing build dependencies (Fedora)..."
        dnf groupinstall -y "Development Tools"
        dnf install -y kernel-devel-$(uname -r) git
    fi
    
    # Create temporary build directory
    local build_dir="/tmp/spdk-drivers-$$"
    mkdir -p "$build_dir"
    cd "$build_dir"
    
    # Try community-maintained igb_uio first
    echo "📦 Downloading community-maintained igb_uio..."
    if git clone https://github.com/wkozaczuk/igb_uio.git; then
        cd igb_uio
        echo "🔨 Building igb_uio module..."
        if make; then
            echo "✅ Installing igb_uio module..."
            make install
            depmod -a
            echo "✅ igb_uio installed successfully"
            cd "$build_dir"
        else
            echo "⚠️  Failed to build community igb_uio, trying legacy DPDK..."
            cd "$build_dir"
            build_legacy_dpdk_igb_uio
        fi
    else
        echo "⚠️  Failed to clone community igb_uio, trying legacy DPDK..."
        build_legacy_dpdk_igb_uio
    fi
    
    # Clean up
    cd /
    rm -rf "$build_dir"
}

# Function to build igb_uio from legacy DPDK
build_legacy_dpdk_igb_uio() {
    echo "📦 Downloading DPDK 20.08 (last version with igb_uio)..."
    
    if wget -q https://fast.dpdk.org/rel/dpdk-20.08.tar.xz; then
        tar xf dpdk-20.08.tar.xz
        cd dpdk-20.08
        
        echo "🔨 Building DPDK with igb_uio..."
        # Use legacy build system
        make config T=x86_64-native-linux-gcc
        make -j$(nproc)
        
        # Install the igb_uio module
        if [ -f "x86_64-native-linux-gcc/kmod/igb_uio.ko" ]; then
            cp x86_64-native-linux-gcc/kmod/igb_uio.ko /lib/modules/$(uname -r)/kernel/drivers/uio/
            depmod -a
            echo "✅ igb_uio from DPDK 20.08 installed successfully"
        else
            echo "❌ Failed to build igb_uio from DPDK"
            return 1
        fi
    else
        echo "❌ Failed to download DPDK 20.08"
        return 1
    fi
}

# Function to setup userspace drivers based on environment
setup_userspace_drivers() {
    local is_virtual=$(detect_virtualization)
    
    echo "🔍 Environment Detection:"
    if [ "$is_virtual" = "true" ]; then
        echo "   📱 Virtualized environment detected"
        echo "   🔧 Will prioritize vfio-pci (requires IOMMU)"
        setup_vfio_drivers
    else
        echo "   🖥️  Bare metal environment detected"
        echo "   🔧 Will setup optimal drivers for bare metal"
        setup_bare_metal_drivers
    fi
}

# Function to setup VFIO drivers for virtualized environments
setup_vfio_drivers() {
    echo "🔧 Setting up VFIO drivers for virtualized environment..."
    
    # Load vfio modules
    modprobe vfio-pci 2>/dev/null || echo "⚠️  vfio-pci module load failed"
    modprobe vfio 2>/dev/null || echo "⚠️  vfio module load failed"
    
    # Load ublk driver for userspace block devices
    modprobe ublk_drv 2>/dev/null || echo "⚠️  ublk_drv module load failed"
    
    # Check IOMMU groups
    local iommu_groups=$(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)
    echo "   📊 IOMMU groups available: $iommu_groups"
    
    if [ "$iommu_groups" -eq 0 ]; then
        echo "   ⚠️  Warning: No IOMMU groups found - vfio-pci may not work"
        echo "   💡 Consider enabling IOMMU in BIOS/hypervisor settings"
    fi
}

# Function to setup drivers for bare metal
setup_bare_metal_drivers() {
    echo "🔧 Setting up userspace drivers for bare metal..."
    
    # Try to load uio_pci_generic (preferred for bare metal)
    if modprobe uio_pci_generic 2>/dev/null; then
        echo "   ✅ uio_pci_generic loaded successfully (no IOMMU required)"
    else
        echo "   ⚠️  uio_pci_generic not available, building igb_uio..."
        
        # Build and install igb_uio for bare metal
        if ! lsmod | grep -q igb_uio; then
            setup_igb_uio
        fi
        
        # Load igb_uio
        if modprobe igb_uio 2>/dev/null; then
            echo "   ✅ igb_uio loaded successfully"
        else
            echo "   ❌ Failed to load igb_uio, falling back to vfio-pci"
            setup_vfio_drivers
        fi
    fi
    
    # Also make vfio-pci available as fallback
    modprobe vfio-pci 2>/dev/null || echo "   ⚠️  vfio-pci fallback not available"
    
    # Load ublk driver for userspace block devices
    modprobe ublk_drv 2>/dev/null || echo "   ⚠️  ublk_drv module load failed"
}

# Function to detect CPU vendor and configure IOMMU if needed
setup_iommu_if_needed() {
    echo "🔍 Checking IOMMU configuration..."
    
    # Check if IOMMU is already enabled
    local iommu_groups=$(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)
    
    if [ "$iommu_groups" -gt 0 ]; then
        echo "   ✅ IOMMU is already enabled ($iommu_groups groups)"
        return 0
    fi
    
    # Check if we're in a virtualized environment
    local is_virtual=$(detect_virtualization)
    if [ "$is_virtual" = "true" ]; then
        echo "   🔍 Virtualized environment - IOMMU may be controlled by hypervisor"
        
        # Check if IOMMU parameters are in kernel command line
        if grep -q "intel_iommu=on\|amd_iommu=on" /proc/cmdline; then
            echo "   ⚠️  IOMMU parameters present but no groups - hypervisor may be blocking"
        else
            echo "   💡 Consider adding IOMMU parameters to kernel command line"
        fi
        return 0
    fi
    
    echo "   ⚠️  IOMMU not enabled - configuring for bare metal..."
    
    # Detect CPU vendor
    local cpu_vendor=$(lscpu | grep "Vendor ID" | awk '{print $3}')
    local iommu_params=""
    
    case $cpu_vendor in
        "GenuineIntel")
            iommu_params="intel_iommu=on iommu=pt"
            echo "   🔧 Detected Intel CPU - will configure with: $iommu_params"
            ;;
        "AuthenticAMD")
            iommu_params="amd_iommu=on iommu=pt"
            echo "   🔧 Detected AMD CPU - will configure with: $iommu_params"
            ;;
        *)
            echo "   ❓ Unknown CPU vendor: $cpu_vendor - using Intel parameters"
            iommu_params="intel_iommu=on iommu=pt"
            ;;
    esac
    
    # Check if IOMMU parameters are already in GRUB
    if grep -q "$iommu_params" /etc/default/grub; then
        echo "   ✅ IOMMU parameters already configured in GRUB"
        echo "   💡 Reboot required to activate IOMMU"
        return 0
    fi
    
    echo "   🔧 Adding IOMMU parameters to GRUB configuration..."
    
    # Backup GRUB configuration
    cp /etc/default/grub /etc/default/grub.backup.$(date +%Y%m%d_%H%M%S)
    
    # Add IOMMU parameters to GRUB_CMDLINE_LINUX_DEFAULT
    if grep -q "GRUB_CMDLINE_LINUX_DEFAULT.*$iommu_params" /etc/default/grub; then
        echo "   ✅ IOMMU parameters already present"
    else
        # Add parameters to existing GRUB_CMDLINE_LINUX_DEFAULT
        sed -i "s/GRUB_CMDLINE_LINUX_DEFAULT=\"\(.*\)\"/GRUB_CMDLINE_LINUX_DEFAULT=\"\1 $iommu_params\"/" /etc/default/grub
        echo "   ✅ IOMMU parameters added to GRUB configuration"
    fi
    
    # Update GRUB
    if command -v update-grub >/dev/null 2>&1; then
        echo "   🔄 Updating GRUB configuration..."
        update-grub
    elif command -v grub2-mkconfig >/dev/null 2>&1; then
        echo "   🔄 Updating GRUB2 configuration..."
        grub2-mkconfig -o /boot/grub2/grub.cfg
    else
        echo "   ⚠️  Could not find GRUB update command"
        echo "   💡 Please manually update GRUB configuration"
    fi
    
    echo "   ✅ GRUB configuration updated successfully"
    echo ""
    echo "   🔄 REBOOT REQUIRED to enable IOMMU"
    echo "   💡 After reboot, IOMMU groups should be available for vfio-pci"
}

# Calculate SPDK-optimized hugepage allocation (2GB minimum, up to 4GB for large systems)
setup_hugepages() {
    echo "🔧 Setting up hugepages for SPDK..."
    
    # Get total memory in GB
    local total_mem_kb=$(grep MemTotal /proc/meminfo | awk '{print $2}')
    local total_mem_gb=$((total_mem_kb / 1024 / 1024))
    
    if [ $total_mem_gb -ge 128 ]; then
        # Large production systems (≥128GB): allocate 4GB for optimal SPDK performance
        hugepage_gb=4
    elif [ $total_mem_gb -ge 64 ]; then
        # Medium-large systems: allocate 3GB
        hugepage_gb=3
    elif [ $total_mem_gb -ge 32 ]; then
        # Medium systems: allocate 2GB (SPDK minimum recommended)
        hugepage_gb=2
    else
        # Smaller systems: allocate 1GB (may impact performance)
        hugepage_gb=1
        echo "   ⚠️  Warning: Only ${total_mem_gb}GB RAM detected. 2GB hugepages recommended for SPDK."
    fi
    
    echo "   📊 System RAM: ${total_mem_gb}GB"
    echo "   🎯 Allocating: ${hugepage_gb}GB hugepages (~$(( hugepage_gb * 100 / total_mem_gb ))% of RAM)"
    
    # Calculate 2MB hugepages needed
    local hugepages_needed=$((hugepage_gb * 1024 / 2))
    
    # Set hugepages
    echo $hugepages_needed > /proc/sys/vm/nr_hugepages
    
    # Mount hugepages
    mkdir -p /dev/hugepages
    mount -t hugetlbfs hugetlbfs /dev/hugepages 2>/dev/null || echo "   ℹ️  Hugepages already mounted"
    
    # Verify hugepages
    local configured_hugepages=$(cat /proc/sys/vm/nr_hugepages)
    local configured_gb=$((configured_hugepages * 2 / 1024))
    
    echo "   ✅ Configured ${configured_hugepages} hugepages (${configured_gb}GB)"
    
    # Make hugepages persistent
    if ! grep -q "vm.nr_hugepages" /etc/sysctl.conf; then
        echo "vm.nr_hugepages=${hugepages_needed}" >> /etc/sysctl.conf
        echo "   ✅ Made hugepages persistent across reboots"
    fi
    
    # Add hugepages mount to fstab
    if ! grep -q hugetlbfs /etc/fstab; then
        echo "hugetlbfs /dev/hugepages hugetlbfs defaults 0 0" >> /etc/fstab
        echo "   ✅ Added hugepages mount to fstab"
    fi
}

# Main setup flow
main() {
    echo ""
    echo "🚀 SPDK CSI Node Setup"
    echo "======================"
    echo ""
    
    # Display configuration
    echo "📋 Configuration:"
    echo "   • Userspace Drivers: $([ "$SKIP_DRIVERS" = true ] && echo "SKIP" || echo "ENABLED")"
    echo "   • Hugepages: $([ "$SKIP_HUGEPAGES" = true ] && echo "SKIP" || echo "ENABLED")"
    echo "   • IOMMU Setup: $([ "$ENABLE_IOMMU" = true ] && echo "ENABLED ⚠️" || echo "DISABLED (safe)")"
    echo ""
    
    # Check if running as root
    if [ "$EUID" -ne 0 ]; then
        echo "❌ This script must be run as root"
        exit 1
    fi
    
    # Warn about IOMMU if enabled
    if [ "$ENABLE_IOMMU" = true ]; then
        echo "⚠️  WARNING: IOMMU setup is ENABLED!"
        echo "   This will modify GRUB and require a reboot."
        echo "   Network connectivity may be affected after reboot."
        echo ""
        echo -n "   Are you sure you have console/IPMI access? (yes/no): "
        read -r confirm
        if [ "$confirm" != "yes" ]; then
            echo "   Aborting for safety. Use without -i flag for safe setup."
            exit 1
        fi
        echo ""
    fi
    
    # Setup userspace drivers based on environment
    if [ "$SKIP_DRIVERS" = false ]; then
        setup_userspace_drivers
        echo ""
    else
        echo "ℹ️  Skipping userspace driver setup (--skip-drivers)"
        echo ""
    fi
    
    # Setup IOMMU only if explicitly enabled
    if [ "$ENABLE_IOMMU" = true ]; then
        echo "🔧 IOMMU setup explicitly requested..."
        setup_iommu_if_needed
        echo ""
    else
        echo "ℹ️  IOMMU setup skipped (use -i/--enable-iommu to enable)"
        log_verbose "Current IOMMU groups: $(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)"
        echo ""
    fi
    
    # Setup hugepages
    if [ "$SKIP_HUGEPAGES" = false ]; then
        setup_hugepages
        echo ""
    else
        echo "ℹ️  Skipping hugepages setup (--skip-hugepages)"
        echo ""
    fi
    
    echo "✅ SPDK CSI node setup completed!"
    echo ""
    echo "📋 Summary:"
    if [ "$SKIP_DRIVERS" = false ]; then
        echo "   🔧 Userspace drivers configured for $([ "$(detect_virtualization)" = "true" ] && echo "virtualized" || echo "bare metal") environment"
    fi
    if [ "$SKIP_HUGEPAGES" = false ]; then
        echo "   📊 Hugepages: $(cat /proc/sys/vm/nr_hugepages) x 2MB = $(($(cat /proc/sys/vm/nr_hugepages) * 2 / 1024))GB"
    fi
    echo "   🔍 IOMMU groups: $(ls /sys/kernel/iommu_groups/ 2>/dev/null | wc -l)"
    echo "   🔐 IOMMU config: $([ "$ENABLE_IOMMU" = true ] && echo "Modified (reboot required)" || echo "Not modified")"
    echo ""
    
    # Check if reboot is needed
    if [ "$ENABLE_IOMMU" = true ]; then
        if grep -q "intel_iommu=on\|amd_iommu=on" /etc/default/grub; then
            echo "🔄 REBOOT REQUIRED to activate IOMMU configuration"
            echo "⚠️  WARNING: Ensure you have console access before rebooting!"
            echo ""
        fi
    fi
    
    echo "🎯 Next steps:"
    if [ "$ENABLE_IOMMU" = true ] && grep -q "intel_iommu=on\|amd_iommu=on" /etc/default/grub; then
        echo "   1. IMPORTANT: Ensure console/IPMI access is available"
        echo "   2. Reboot to activate IOMMU: sudo reboot"
        echo "   3. After reboot, verify network connectivity"
        echo "   4. Deploy SPDK CSI driver: kubectl apply -f flint-csi-driver-chart/"
    else
        echo "   1. Deploy SPDK CSI driver: kubectl apply -f flint-csi-driver-chart/"
        echo "   2. Verify driver status: kubectl get pods -n flint-system"
    fi
}

# Parse arguments first
parse_args "$@"

# Run main function
main
