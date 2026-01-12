#!/bin/bash
# NVMe-oF Performance Tuning Commands

echo "=== NVMe-oF Performance Tuning ==="
echo ""

echo "1. Increase NVMe queue depth (on both nodes):"
echo "   sudo modprobe -r nvme_tcp"
echo "   sudo modprobe nvme_tcp io_queue_depth=256"
echo ""

echo "2. Increase max I/O size (on both nodes):"
echo "   echo 1024 | sudo tee /sys/block/nvme*/queue/max_sectors_kb"
echo ""

echo "3. Enable jumbo frames (if your network supports 9000 MTU):"
echo "   # On both nodes:"
echo "   sudo ip link set <interface> mtu 9000"
echo "   # Example: sudo ip link set ens160 mtu 9000"
echo ""

echo "4. Optimize TCP for NVMe-oF (on both nodes):"
cat <<'EOF'
   sudo sysctl -w net.core.rmem_max=134217728
   sudo sysctl -w net.core.wmem_max=134217728
   sudo sysctl -w net.ipv4.tcp_rmem="4096 87380 67108864"
   sudo sysctl -w net.ipv4.tcp_wmem="4096 65536 67108864"
   sudo sysctl -w net.ipv4.tcp_mem="67108864 67108864 67108864"
   sudo sysctl -w net.core.netdev_max_backlog=5000
EOF
echo ""

echo "5. To make persistent, add to /etc/sysctl.conf:"
cat <<'EOF'
   net.core.rmem_max=134217728
   net.core.wmem_max=134217728
   net.ipv4.tcp_rmem=4096 87380 67108864
   net.ipv4.tcp_wmem=4096 65536 67108864
   net.ipv4.tcp_mem=67108864 67108864 67108864
   net.core.netdev_max_backlog=5000
EOF
echo ""

echo "6. For best performance, use fio instead of dd for testing:"
echo "   fio --name=test --rw=write --bs=4k --iodepth=64 --numjobs=4 --size=1G --filename=/data/test"
echo ""

echo "Expected improvements:"
echo "  - Queue depth 256: +50-100% throughput"
echo "  - Jumbo frames: +20-30% throughput"
echo "  - TCP tuning: +10-20% throughput"
echo "  - Combined: 3-6 GB/s possible over NVMe-oF TCP"
