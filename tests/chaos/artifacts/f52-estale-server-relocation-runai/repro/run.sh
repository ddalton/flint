#!/bin/sh
# Runs inside a privileged Linux container. Proves the F52 mechanism.
set -e
apk add -q gcc musl-dev e2fsprogs >/dev/null 2>&1 || (apt-get -qq update && apt-get -qq install -y gcc libc6-dev e2fsprogs >/dev/null)
cc -O -o /fhtest /work/fhtest.c

dd if=/dev/zero of=/img bs=1M count=64 status=none
mkfs.ext4 -q /img
mkdir -p /mnt/vol

echo "=== PHASE 1: mount, create file, mint handle, resolve WARM ==="
mount -o loop /img /mnt/vol
mkdir -p /mnt/vol/pgdata/global
echo "hello-f52" > /mnt/vol/pgdata/global/1262
/fhtest mint /mnt/vol/pgdata/global/1262 /handle.hex
/fhtest resolve /mnt/vol /handle.hex

echo ""
echo "=== PHASE 2: umount + remount (cold dcache, same fs image), resolve COLD ==="
umount /mnt/vol
mount -o loop /img /mnt/vol
/fhtest resolve /mnt/vol /handle.hex

echo ""
echo "=== PHASE 3: same cold mount, after ONE path lookup of the file (dcache warmed) ==="
stat /mnt/vol/pgdata/global/1262 >/dev/null
/fhtest resolve /mnt/vol /handle.hex

echo ""
echo "=== PHASE 4: THE FIX, against a fresh cold mount (trust gate + identity walk) ==="
umount /mnt/vol
mount -o loop /img /mnt/vol
/fhtest resolve-fixed /mnt/vol /handle.hex

echo ""
echo "=== PHASE 5: THE FIX, warm path (trust gate passes, no walk) ==="
/fhtest resolve-fixed /mnt/vol /handle.hex

echo ""
echo "=== PHASE 6: THE FIX, unlinked file on a cold mount -> STALE, never a foreign path ==="
umount /mnt/vol
mount -o loop /img /mnt/vol
rm /mnt/vol/pgdata/global/1262
/fhtest resolve-fixed /mnt/vol /handle.hex || true

umount /mnt/vol
echo ""
echo "kernel: $(uname -r)"
