#!/bin/bash

# Build script for SPDK Dashboard images
# Usage: ./scripts/build-dashboard.sh [REGISTRY] [TAG]

set -e

REGISTRY=${1:-"flint"}
TAG=${2:-"latest"}

echo "Building SPDK Dashboard images with registry: $REGISTRY, tag: $TAG"

# Build backend image
echo "=== Building Dashboard Backend ==="
docker build -t "${REGISTRY}/spdk-dashboard-backend:${TAG}" \
  -f docker/Dockerfile.dashboard-backend .

echo "✅ Backend image built: ${REGISTRY}/spdk-dashboard-backend:${TAG}"

# Build frontend image
echo "=== Building Dashboard Frontend ==="
cd ../spdk-dashboard
docker build -t "${REGISTRY}/spdk-dashboard-frontend:${TAG}" \
  -f Dockerfile.frontend .

echo "✅ Frontend image built: ${REGISTRY}/spdk-dashboard-frontend:${TAG}"

# Return to original directory
cd ../spdk-csi-driver

echo ""
echo "🎉 Dashboard images built successfully!"
echo "Backend:  ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "Frontend: ${REGISTRY}/spdk-dashboard-frontend:${TAG}"
echo ""
echo "To push to registry:"
echo "docker push ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "docker push ${REGISTRY}/spdk-dashboard-frontend:${TAG}" 