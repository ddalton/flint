#!/bin/bash
set -u
BUCKET=flint-tlc-sentinel-20260914
exec > /var/log/tlcbox-bootstrap.log 2>&1
shutdown -h +720 "flint tlc box: 12 h cap"
fail() { echo "BOOTSTRAP FAILED: $*"; aws s3 cp /var/log/tlcbox-bootstrap.log "s3://$BUCKET/out/BOOTSTRAP-FAILED.log"; shutdown -h now; exit 1; }
dnf install -y java-21-amazon-corretto-headless xfsprogs || fail dnf
# the instance-store disk BY MODEL, never by /dev name (names are non-deterministic)
DEV=""; N=0
for d in /sys/block/nvme*n1; do
  m=$(tr -d '\n' < "$d/device/model" | sed 's/ *$//')
  echo "$(basename $d): model=[$m] size=$(cat $d/size)"
  if [ "$m" = "Amazon EC2 NVMe Instance Storage" ]; then DEV=/dev/$(basename $d); N=$((N+1)); fi
done
[ "$N" = 1 ] || fail "expected exactly one instance-store disk, found $N"
[ -z "$(lsblk -n -o MOUNTPOINT "$DEV" | tr -d ' \n')" ] || fail "$DEV is mounted"
[ "$(lsblk -n "$DEV" | wc -l)" = 1 ] || fail "$DEV has partitions"
mkfs.xfs -f "$DEV" || fail mkfs
mkdir -p /data && mount -o noatime "$DEV" /data || fail mount
for i in $(seq 1 20); do aws s3 cp --recursive "s3://$BUCKET/payload/" /data/payload/ && break; sleep 15; done
cd /data/payload && sha256sum -c SHA256SUMS || fail "payload checksum"
java -version; nproc; free -g; df -h /data
aws s3 cp /var/log/tlcbox-bootstrap.log "s3://$BUCKET/out/bootstrap.log"
systemd-run --unit=tlcbox --property=StandardOutput=file:/data/runner.log --property=StandardError=file:/data/runner.log bash /data/payload/runner.sh "$BUCKET" || fail systemd-run
