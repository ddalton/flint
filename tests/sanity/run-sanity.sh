#!/bin/bash
set -e

# CSI Sanity Test Runner for Flint
# This script runs the official CSI sanity test suite against the Flint driver

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CSI_SOCKET="${CSI_SOCKET:-/tmp/flint-csi.sock}"
STAGING_DIR="${STAGING_DIR:-/tmp/flint-staging}"
MOUNT_DIR="${MOUNT_DIR:-/tmp/flint-mount}"

echo "==================================================================="
echo "Flint CSI Driver - Sanity Test Suite"
echo "==================================================================="
echo "CSI Socket: $CSI_SOCKET"
echo "Staging Dir: $STAGING_DIR"
echo "Mount Dir: $MOUNT_DIR"
echo "==================================================================="

# Check if csi-sanity is installed
if ! command -v csi-sanity &> /dev/null; then
    echo "❌ csi-sanity not found"
    echo ""
    echo "Install it with:"
    echo "  go install github.com/kubernetes-csi/csi-test/cmd/csi-sanity@latest"
    exit 1
fi

echo "✅ csi-sanity found: $(which csi-sanity)"

# Create staging and mount directories
mkdir -p "$STAGING_DIR" "$MOUNT_DIR"
echo "✅ Created test directories"

# Check if driver is running
if [ ! -S "$CSI_SOCKET" ]; then
    echo "❌ CSI socket not found at $CSI_SOCKET"
    echo ""
    echo "Start the driver first:"
    echo "  CSI_MODE=node CSI_ENDPOINT=unix://$CSI_SOCKET \\"
    echo "  NODE_ID=test-node SPDK_RPC_URL=unix:///var/tmp/spdk.sock \\"
    echo "  cargo run --bin csi-driver"
    exit 1
fi

echo "✅ CSI socket found: $CSI_SOCKET"
echo ""
echo "Running CSI Sanity tests..."
echo "-------------------------------------------------------------------"

# Run sanity tests
csi-sanity \
  --csi.endpoint="unix://$CSI_SOCKET" \
  --csi.stagingdir="$STAGING_DIR" \
  --csi.mountdir="$MOUNT_DIR" \
  --csi.testvolumesize=1073741824 \
  --ginkgo.v

RESULT=$?

echo "-------------------------------------------------------------------"
if [ $RESULT -eq 0 ]; then
    echo "✅ CSI Sanity tests PASSED"
else
    echo "❌ CSI Sanity tests FAILED (exit code: $RESULT)"
fi

# Cleanup
echo ""
echo "Cleaning up test directories..."
rm -rf "$STAGING_DIR" "$MOUNT_DIR"

exit $RESULT

