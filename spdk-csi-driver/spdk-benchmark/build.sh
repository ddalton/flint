#!/bin/bash
set -e

# Build SPDK Native Benchmark Docker Image
echo "═══════════════════════════════════════════════════════"
echo "Building SPDK Native Benchmark"
echo "═══════════════════════════════════════════════════════"

# Move to parent directory for Docker context
cd "$(dirname "$0")/.."

# Build the Docker image
echo "Building Docker image..."
docker build -f docker/Dockerfile.spdk-benchmark -t dilipdalton/spdk-benchmark:latest .

echo ""
echo "═══════════════════════════════════════════════════════"
echo "✓ Build complete!"
echo "═══════════════════════════════════════════════════════"
echo ""
echo "To push to registry:"
echo "  docker push dilipdalton/spdk-benchmark:latest"
echo ""
echo "To run locally:"
echo "  docker run --rm --privileged -v /dev:/dev -v /sys:/sys -v /mnt/huge:/mnt/huge dilipdalton/spdk-benchmark:latest"
echo ""
