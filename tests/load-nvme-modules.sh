#!/bin/bash
# Script to load NVMe kernel modules on Kubernetes nodes

echo "========================================"
echo "NVMe Module Loader"
echo "========================================"
echo ""

# Check if modules exist
echo "Checking for NVMe modules..."
MODULE_PATH="/lib/modules/$(uname -r)"

if [ ! -d "$MODULE_PATH" ]; then
    echo "ERROR: Kernel modules directory not found: $MODULE_PATH"
    exit 1
fi

echo "Kernel version: $(uname -r)"
echo ""

# Function to load a module
load_module() {
    local module=$1
    echo -n "Loading $module... "
    
    if lsmod | grep -q "^${module} "; then
        echo "already loaded"
        return 0
    fi
    
    if modprobe "$module" 2>/dev/null; then
        echo "SUCCESS"
        return 0
    else
        echo "FAILED (may not be available)"
        return 1
    fi
}

# Load NVMe modules in order
echo "Loading NVMe kernel modules:"
load_module nvme_core
load_module nvme
load_module nvme_fabrics
load_module nvme_tcp

echo ""
echo "Verifying loaded modules:"
lsmod | grep nvme || echo "WARNING: No NVMe modules found!"

echo ""
echo "NVMe devices:"
ls -la /sys/class/nvme* 2>/dev/null || echo "No NVMe devices found yet"

echo ""
echo "========================================"
echo "Done!"
echo "========================================"
