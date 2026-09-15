#!/usr/bin/env bash
# What the lean tree's loop-mounted image costs, measured on a node (run AS
# ROOT on the node, e.g. scripts/nodesh.sh; needs fio, python3, losetup).
#
# THREE LAYOUTS of the same tree, each on the same backing filesystem:
#   plain    a directory                         (sizeLimitGib: 0)
#   loop     a sparse image, `mount -o loop`     (today's quota: direct-io OFF)
#   loopdio  the same image on a loop device with --direct-io=on
# The image is built exactly as s3csi/quota.rs builds it: a sparse file of
# IMG_GIB, `mkfs.ext4 -F -q -m 0 -E lazy_itable_init=1,lazy_journal_init=1`,
# mounted noatime.
#
# ON TWO BACKINGS: the node's root filesystem, where the plugin keeps trees
# as deployed (BACKINGS=root), and a scratch ext4 on a local NVMe device
# (NVME=/dev/nvme1n1, BACKINGS=nvme) that no disk cap can flatten. The NVMe is
# REFORMATTED.
#
# WORKLOADS, each repeated N times with caches dropped before every run:
#   seqw_fsync  one file of SIZE, 1 MiB buffered writes, fsync at the end
#   seqw_buf    the same with no fsync: the page-cache path an agent feels
#   small       FILES files of 16 KiB, each written to a temp name and renamed
#               (the checkout shape), then one sync of the filesystem
#   randw_sync  4 KiB random writes, fsync after each, for 15 s (git/db-ish)
#   seqr_cold   read the seqw file back with every cache dropped
# Every time is wall clock around the whole operation, fsync included.
#
# CONTROLS, asserted before each arm measures anything: plain is not a mount
# point; loop's device reports dio=0; loopdio's reports dio=1. An arm that is
# not the layout it claims prints VOID and measures nothing.
#
# Output: one JSON line per run on stdout (backing, layout, workload, rep,
# secs, mib_s or iops), and LAZYINIT lines: how much a fresh image writes to
# its backing file on its own after mount (sampled for LAZY_SECS).
set -uo pipefail
N=${N:-5}
BACKINGS=${BACKINGS:-"root nvme"}
ROOT_BASE=${ROOT_BASE:-/var/lib/kubelet/flint-loop-bench}
NVME=${NVME:-/dev/nvme1n1}
NVME_MNT=${NVME_MNT:-/mnt/flint-loop-bench}
IMG_GIB=${IMG_GIB:-20}
LAZY_SECS=${LAZY_SECS:-60}

drop() { sync; echo 3 > /proc/sys/vm/drop_caches; }
now() { date +%s.%N; }
elapsed() { python3 -c "print(round($2 - $1, 3))"; }
emit() { echo "{\"backing\":\"$1\",\"layout\":\"$2\",\"workload\":\"$3\",\"rep\":$4,\"secs\":$5,\"$6\":$7}"; }

size_of() { case "$1" in root) echo "${ROOT_SIZE_MIB:-256}";; nvme) echo "${NVME_SIZE_MIB:-2048}";; esac; }
files_of() { case "$1" in root) echo "${ROOT_FILES:-5000}";; nvme) echo "${NVME_FILES:-20000}";; esac; }

TREE=""; LOOPDEV=""
arm_up() { # base layout
    local base=$1 layout=$2
    TREE="$base/$layout"; LOOPDEV=""
    case "$layout" in
        plain) mkdir -p "$TREE" ;;
        loop|loopdio)
            rm -f "$base/$layout.img"
            truncate -s "${IMG_GIB}G" "$base/$layout.img"
            mkfs.ext4 -F -q -m 0 -E lazy_itable_init=1,lazy_journal_init=1 "$base/$layout.img"
            mkdir -p "$TREE"
            if [ "$layout" = loop ]; then
                mount -o loop,noatime "$base/$layout.img" "$TREE"
                LOOPDEV=$(findmnt -n -o SOURCE "$TREE")
            else
                LOOPDEV=$(losetup --find --show --direct-io=on "$base/$layout.img")
                mount -o noatime "$LOOPDEV" "$TREE"
            fi
            ;;
    esac
}
arm_check() { # layout — prints nothing when the arm is what it claims
    local dio
    case "$1" in
        plain) mountpoint -q "$TREE" && echo "VOID: plain tree $TREE is a mount point" ;;
        loop|loopdio)
            dio=$(cat "/sys/block/$(basename "$LOOPDEV")/loop/dio" 2>/dev/null)
            [ "$1" = loop ] && [ "$dio" != 0 ] && echo "VOID: loop arm has dio='$dio'"
            [ "$1" = loopdio ] && [ "$dio" != 1 ] && echo "VOID: loopdio arm has dio='$dio'"
            ;;
    esac
}
arm_down() { # base layout
    if [ -n "$LOOPDEV" ]; then
        umount "$TREE"
        losetup -d "$LOOPDEV" 2>/dev/null
        rm -f "$1/$2.img"
    fi
    rm -rf "$TREE"
}

lazyinit() { # base — a fresh image's own writes after mount
    local img="$1/lazy.img" t0 a
    rm -f "$img"; truncate -s "${IMG_GIB}G" "$img"
    mkfs.ext4 -F -q -m 0 -E lazy_itable_init=1,lazy_journal_init=1 "$img"
    mkdir -p "$1/lazy"; mount -o loop,noatime "$img" "$1/lazy"
    a=$(du -k "$img" | cut -f1); echo "LAZYINIT $2 t=0 allocated_kib=$a"
    for t in $(seq 10 10 "$LAZY_SECS"); do sleep 10; echo "LAZYINIT $2 t=$t allocated_kib=$(du -k "$img" | cut -f1)"; done
    umount "$1/lazy"; rm -rf "$img" "$1/lazy"
}

small_files() { # dir count
    python3 - "$1" "$2" <<'PY'
import os, sys
d, n = sys.argv[1], int(sys.argv[2])
buf = b"x" * 16384
for i in range(n):
    sub = os.path.join(d, "d%03d" % (i // 1000))
    if i % 1000 == 0:
        os.makedirs(sub, exist_ok=True)
    tmp = os.path.join(sub, ".f%d.tmp" % i)
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    os.write(fd, buf)
    os.close(fd)
    os.rename(tmp, os.path.join(sub, "f%d" % i))
os.sync()
PY
}

for backing in $BACKINGS; do
    case "$backing" in
        root) base=$ROOT_BASE; mkdir -p "$base" ;;
        nvme)
            base=$NVME_MNT
            mountpoint -q "$base" && umount "$base"
            mkfs.ext4 -F -q "$NVME" && mkdir -p "$base" && mount -o noatime "$NVME" "$base" || { echo "VOID: cannot format $NVME"; continue; }
            ;;
    esac
    echo "BACKING $backing $(findmnt -n -o SOURCE,FSTYPE --target "$base") free=$(df -h --output=avail "$base" | tail -1)"
    lazyinit "$base" "$backing"
    sz=$(size_of "$backing"); nf=$(files_of "$backing")
    for layout in plain loop loopdio; do
        arm_up "$base" "$layout"
        v=$(arm_check "$layout"); if [ -n "$v" ]; then echo "$v"; arm_down "$base" "$layout"; continue; fi
        # A fresh image's lazy init would run under the first reps and bill
        # them for it; lazyinit above measures that cost on its own. Wait
        # until the image stops growing (three unchanged 10 s samples).
        if [ "$layout" != plain ]; then
            prev=-1; same=0; waited=0
            while [ $same -lt 3 ] && [ $waited -lt "${SETTLE_MAX:-900}" ]; do
                cur=$(du -k "$base/$layout.img" | cut -f1)
                [ "$cur" = "$prev" ] && same=$((same + 1)) || same=0
                prev=$cur; sleep 10; waited=$((waited + 10))
            done
            echo "SETTLED $backing $layout after ${waited}s at allocated_kib=$prev"
        fi
        for rep in $(seq 1 "$N"); do
            f="$TREE/seq.bin"
            rm -f "$f"; drop; t0=$(now)
            fio --name=w --filename="$f" --rw=write --bs=1M --size="${sz}M" --ioengine=psync --end_fsync=1 >/dev/null 2>&1
            t=$(elapsed "$t0" "$(now)"); emit "$backing" "$layout" seqw_fsync "$rep" "$t" mib_s "$(python3 -c "print(round($sz/$t,1))")"

            drop; t0=$(now)
            fio --name=r --filename="$f" --rw=read --bs=1M --size="${sz}M" --ioengine=psync >/dev/null 2>&1
            t=$(elapsed "$t0" "$(now)"); emit "$backing" "$layout" seqr_cold "$rep" "$t" mib_s "$(python3 -c "print(round($sz/$t,1))")"

            rm -f "$f"; drop; t0=$(now)
            fio --name=w --filename="$f" --rw=write --bs=1M --size="${sz}M" --ioengine=psync >/dev/null 2>&1
            t=$(elapsed "$t0" "$(now)"); emit "$backing" "$layout" seqw_buf "$rep" "$t" mib_s "$(python3 -c "print(round($sz/$t,1))")"
            rm -f "$f"; sync

            rm -rf "$TREE/small"; mkdir -p "$TREE/small"; drop; t0=$(now)
            small_files "$TREE/small" "$nf"
            t=$(elapsed "$t0" "$(now)"); emit "$backing" "$layout" small "$rep" "$t" files_s "$(python3 -c "print(round($nf/$t))")"
            rm -rf "$TREE/small"; sync

            drop
            iops=$(fio --name=rw --filename="$TREE/rand.bin" --rw=randwrite --bs=4k --size=64M --ioengine=psync --fsync=1 \
                --time_based --runtime=15 --output-format=json 2>/dev/null | python3 -c "import json,sys; print(round(json.load(sys.stdin)['jobs'][0]['write']['iops']))")
            emit "$backing" "$layout" randw_sync "$rep" 15 iops "${iops:-0}"
            rm -f "$TREE/rand.bin"; sync
        done
        arm_down "$base" "$layout"
    done
    if [ "$backing" = nvme ]; then umount "$base"; wipefs -a -q "$NVME"; fi
    [ "$backing" = root ] && rm -rf "$ROOT_BASE"
done
echo "DONE"
