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
fi

touch /var/tmp/spdk.ready
echo "SPDK ready (PID $SPDK_PID)"

wait "$SPDK_PID"
EXIT_CODE=$?
echo "SPDK exited with code $EXIT_CODE"
rm -f /var/tmp/spdk.ready
exit "$EXIT_CODE"
