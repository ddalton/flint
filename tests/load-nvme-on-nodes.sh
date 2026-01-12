#!/bin/bash
# Load NVMe modules on both Kubernetes nodes

echo "========================================"
echo "Loading NVMe modules on cluster nodes"
echo "========================================"

# Commands to run on each node
LOAD_CMDS="
echo 'Loading NVMe modules...'
sudo modprobe nvme_core
sudo modprobe nvme
sudo modprobe nvme_fabrics
sudo modprobe nvme_tcp
echo 'Verifying modules:'
lsmod | grep nvme
echo 'Making persistent across reboots:'
sudo mkdir -p /etc/modules-load.d
echo 'nvme_core' | sudo tee -a /etc/modules-load.d/nvme.conf
echo 'nvme' | sudo tee -a /etc/modules-load.d/nvme.conf
echo 'nvme_fabrics' | sudo tee -a /etc/modules-load.d/nvme.conf
echo 'nvme_tcp' | sudo tee -a /etc/modules-load.d/nvme.conf
echo 'Done!'
"

echo ""
echo "Run these commands on flnt-4-46-m1:"
echo "------------------------------------"
echo "$LOAD_CMDS"
echo ""
echo "Run these commands on flnt-4-46-w1:"
echo "------------------------------------"
echo "$LOAD_CMDS"
echo ""
echo "========================================"
