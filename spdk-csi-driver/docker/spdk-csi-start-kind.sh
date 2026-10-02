#!/bin/bash
set -e

echo "SPDK Pre-start cleanup..."
rm -f /var/tmp/spdk.sock /var/tmp/spdk.ready

echo "Starting SPDK v26.01 in --wait-for-rpc mode (PID: $$)"
echo "Will minimize memory pools before subsystem init"

/usr/local/bin/spdk_tgt -r /var/tmp/spdk.sock -L all --json /etc/spdk/config.json --wait-for-rpc "$@" &
SPDK_PID=$!

_shutdown() {
    echo "[$(date)] spdk-csi-start-kind: forwarding $1 to SPDK (pid $SPDK_PID)"
    kill -s "$1" "$SPDK_PID" 2>/dev/null || true
    wait "$SPDK_PID" 2>/dev/null || true
    exit 0
}
trap '_shutdown SIGTERM' SIGTERM
trap '_shutdown SIGINT'  SIGINT

echo "Waiting for SPDK RPC socket..."
SOCKET_WAIT=0
while [ ! -S /var/tmp/spdk.sock ]; do
    sleep 0.1
    SOCKET_WAIT=$((SOCKET_WAIT + 1))
    if [ "$SOCKET_WAIT" -ge 300 ]; then
        echo "ERROR: SPDK socket did not appear after 30s"
        kill "$SPDK_PID" 2>/dev/null || true
        exit 1
    fi
    if ! kill -0 "$SPDK_PID" 2>/dev/null; then
        echo "ERROR: SPDK process died before socket appeared"
        exit 1
    fi
done
echo "SPDK RPC socket ready"

RPC="python3 /usr/local/scripts/rpc.py -s /var/tmp/spdk.sock"

echo "Minimizing ioBuf pools..."
$RPC iobuf_set_options --small-pool-count 4096 --large-pool-count 1024

echo "Minimizing iSCSI pools..."
$RPC iscsi_set_options -a 1 -c 1 -q 1 -x 1 -k 1 -u 24 -j 1 -z 1

echo "Triggering SPDK subsystem initialization..."
$RPC framework_start_init

echo "Waiting for SPDK subsystems to initialize..."
INIT_WAIT=0
until $RPC framework_wait_init 2>/dev/null; do
    sleep 0.5
    INIT_WAIT=$((INIT_WAIT + 1))
    if [ "$INIT_WAIT" -ge 120 ]; then
        echo "ERROR: SPDK subsystems did not initialize after 60s"
        kill "$SPDK_PID" 2>/dev/null || true
        exit 1
    fi
    if ! kill -0 "$SPDK_PID" 2>/dev/null; then
        echo "ERROR: SPDK process died during initialization"
        exit 1
    fi
done
echo "SPDK subsystems initialized"

if [ -n "$VIRTUAL_DISK_SIZE_MB" ] && [ "$VIRTUAL_DISK_SIZE_MB" -gt 0 ]; then
    LVS_NAME="${VIRTUAL_DISK_LVS_NAME:-lvs_kind}"
    BDEV_NAME="malloc_kind_disk"

    echo "Creating malloc bdev: $BDEV_NAME (${VIRTUAL_DISK_SIZE_MB}MB)"
    $RPC bdev_malloc_create -b "$BDEV_NAME" "$VIRTUAL_DISK_SIZE_MB" 512

    if ! $RPC bdev_lvol_get_lvstores 2>/dev/null | grep -q "\"$LVS_NAME\""; then
        echo "Creating LVS: $LVS_NAME on $BDEV_NAME"
        $RPC bdev_lvol_create_lvstore "$BDEV_NAME" "$LVS_NAME" --cluster-sz 1048576
    else
        echo "LVS $LVS_NAME already exists, skipping creation"
    fi

    echo "Virtual disk ready: $LVS_NAME on $BDEV_NAME (malloc-backed)"
elif [ -n "$VIRTUAL_DISK_DEVICE" ]; then
    # A REAL block device (a partition the rig binds into the kind node),
    # opened with io_uring like the driver's kernel-bound fallback. Unlike
    # malloc it survives an spdk-tgt restart: the lvstore on it is LOADED,
    # never recreated, so a killed-and-restarted leg comes back with its
    # data (what tests-replica-rebuild exercises).
    LVS_NAME="${VIRTUAL_DISK_LVS_NAME:-lvs_kind}"
    # %NODE% names this node's own volume when every node sees one shared
    # /dev (the rig binds the host's /dev into each kind node).
    VIRTUAL_DISK_DEVICE="$(printf '%s' "$VIRTUAL_DISK_DEVICE" | sed "s/%NODE%/${NODE_NAME:-}/g")"
    if [ ! -b "$VIRTUAL_DISK_DEVICE" ]; then
        echo "ERROR: VIRTUAL_DISK_DEVICE $VIRTUAL_DISK_DEVICE is not a block device"
        kill "$SPDK_PID" 2>/dev/null || true
        exit 1
    fi
    # SPDK's uring bdev probes /sys/block/<basename>/queue/zoned and fails
    # when that is absent, so the device must be opened under its KERNEL
    # name, and must be a whole block device (a partition has no
    # /sys/block entry; an LVM volume, dm-N, does). The rig binds it at a
    # fixed path; resolve the kernel name from the device number.
    MAJMIN="$(( 0x$(stat -Lc %t "$VIRTUAL_DISK_DEVICE") )):$(( 0x$(stat -Lc %T "$VIRTUAL_DISK_DEVICE") ))"
    KNAME="$(basename "$(readlink -f "/sys/dev/block/$MAJMIN")")"
    if [ -z "$KNAME" ] || [ ! -e "/sys/block/$KNAME" ]; then
        echo "ERROR: $VIRTUAL_DISK_DEVICE ($MAJMIN -> '$KNAME') is not a whole block device in /sys/block (a partition? use an LVM volume)"
        kill "$SPDK_PID" 2>/dev/null || true
        exit 1
    fi
    if [ ! -b "/dev/$KNAME" ]; then
        mknod "/dev/$KNAME" b "${MAJMIN%%:*}" "${MAJMIN##*:}"
    fi
    VIRTUAL_DISK_DEVICE="/dev/$KNAME"
    BDEV_NAME="uring_$KNAME"
    echo "Creating uring bdev: $BDEV_NAME on $VIRTUAL_DISK_DEVICE"
    $RPC bdev_uring_create "$VIRTUAL_DISK_DEVICE" "$BDEV_NAME"
    # An existing lvstore is loaded by the bdev examine, asynchronously.
    # Deciding "absent" before examine finishes would create a NEW lvstore
    # over the old one -- wiping every volume on the restart this path
    # exists for. Wait for examine, then look.
    $RPC bdev_wait_for_examine
    if ! $RPC bdev_lvol_get_lvstores 2>/dev/null | grep -q "\"$LVS_NAME\""; then
        echo "Creating LVS: $LVS_NAME on $BDEV_NAME"
        $RPC bdev_lvol_create_lvstore "$BDEV_NAME" "$LVS_NAME" --cluster-sz 1048576
    else
        echo "LVS $LVS_NAME loaded from $VIRTUAL_DISK_DEVICE, not recreated"
    fi
    echo "Disk ready: $LVS_NAME on $BDEV_NAME (device-backed)"
fi

touch /var/tmp/spdk.ready
echo "SPDK ready (PID $SPDK_PID)"

wait "$SPDK_PID"
EXIT_CODE=$?
echo "SPDK exited with code $EXIT_CODE"
rm -f /var/tmp/spdk.ready
exit "$EXIT_CODE"
