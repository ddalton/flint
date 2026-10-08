#!/usr/bin/env bash
# wipe-disks.sh -- between two drivers (plan §5): give every node's instance
# store back to the kernel nvme driver, discard it whole, zero its first and
# last 64 MiB, and verify those read zero. Run AFTER the previous driver is
# fully uninstalled; it refuses a disk that anything still holds open.
#
# The discard returns the flash to the same state for every driver; it does
# NOT make the disk read zero -- a trimmed block on Nitro instance storage
# reads back as noise (first run, bench3 2026-10-08). The explicit zeroing is
# what removes every driver's on-disk label (SPDK blobstore super block, Ceph
# bluestore label, LVM, the GPT backup at the end), so no driver can find and
# adopt the previous one's state.
#
#   CONFIRM_WIPE=yes ./wipe-disks.sh
#
# A userspace NVMe driver (Longhorn v2's `nvme` disk driver) unbinds the
# kernel driver, so the device has no /dev node until it is rebound; the
# rebind is done by PCI address, found by the controller's vendor/device.
set -euo pipefail
. "$(cd "$(dirname "$0")/.." && pwd)/lib/host.sh"
[ "${CONFIRM_WIPE:-}" = yes ] || fail "set CONFIRM_WIPE=yes: this DISCARDS every node's instance store"
samplers_up

for n in $(nodes); do
  step "$n: give the instance store back to the kernel nvme driver"
  # Amazon instance-store controllers: vendor 0x1d0f, device 0xcd01.
  hostexec "$n" '
    for d in /sys/bus/pci/devices/*; do
      [ "$(cat $d/vendor)" = 0x1d0f ] && [ "$(cat $d/device)" = 0xcd01 ] || continue
      bdf=$(basename $d)
      drv=$(basename "$(readlink -f $d/driver 2>/dev/null)" 2>/dev/null || true)
      if [ "$drv" != nvme ]; then
        echo "$bdf: bound to ${drv:-nothing}; rebinding to nvme"
        [ -n "$drv" ] && echo "$bdf" > "$d/driver/unbind"
        echo "" > "$d/driver_override" 2>/dev/null || true
        echo "$bdf" > /sys/bus/pci/drivers/nvme/bind
      fi
    done'
  sleep 3
  read -r dev bdf byid <<<"$(instance_store "$n")"
  [ -n "${dev:-}" ] || fail "$n: no '$DISK_MODEL' block device after rebind"
  step "$n: discard $dev ($bdf)"
  hostexec "$n" "
    set -e
    h=\$(ls /sys/block/$(basename "$dev")/holders)
    [ -z \"\$h\" ] || { echo 'holders: '\$h; exit 1; }
    if command -v fuser >/dev/null && fuser -s $dev; then echo 'open by a process'; exit 1; fi
    wipefs -a $dev >/dev/null
    blkdiscard $dev
    mib=\$(( \$(blockdev --getsize64 $dev) / 1048576 ))
    dd if=/dev/zero of=$dev bs=1M count=64 oflag=direct status=none
    dd if=/dev/zero of=$dev bs=1M count=64 seek=\$((mib - 64)) oflag=direct status=none
    for skip in 0 \$((mib - 64)); do
      nz=\$(dd if=$dev bs=1M count=64 skip=\$skip iflag=direct status=none | od -An -v -tx1 | tr -d ' \n' | tr -d 0 | wc -c)
      [ \"\$nz\" = 0 ] || { echo \"64 MiB at MiB \$skip not zero after zeroing\"; exit 1; }
    done
    echo clean" || fail "$n: $dev could not be wiped"
done
