set -euo pipefail
dev=""
for d in /dev/nvme*n1; do
  m=$(cat /sys/block/$(basename $d)/device/model 2>/dev/null | xargs)
  if [ "$m" = "Amazon EC2 NVMe Instance Storage" ] && ! lsblk -no MOUNTPOINT "$d" | grep -q .; then dev=$d; fi
done
[ -n "$dev" ] || { echo "NO INSTANCE STORE"; lsblk; exit 1; }
echo "data disk $dev ($(lsblk -dno SIZE $dev), $(readlink -f /sys/block/$(basename $dev)/device | grep -o '0000:[0-9a-f:.]*' | tail -1))"
mountpoint -q /mnt/nvme || { mkfs.xfs -f -q "$dev"; mkdir -p /mnt/nvme; mount "$dev" /mnt/nvme; }
cd /tmp && curl -sSfLO https://s3.amazonaws.com/mountpoint-s3-release/1.24.0/x86_64/mount-s3-1.24.0-x86_64.rpm && dnf install -y -q ./mount-s3-1.24.0-x86_64.rpm >/dev/null
aws configure set default.s3.max_concurrent_requests 32
mkdir -p /mnt/nvme/rig && aws s3 cp --recursive --quiet s3://flint-lean-door-20260924/_rig/ /mnt/nvme/rig/ && chmod +x /mnt/nvme/rig/flint-sync* /mnt/nvme/rig/doors.sh
sha256sum /mnt/nvme/rig/flint-sync* | cut -c1-16
mount-s3 --version; aws --version; uname -r; nproc; free -g | head -2; df -h /mnt/nvme | tail -1
