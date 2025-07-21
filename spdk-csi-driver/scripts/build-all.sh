#!/bin/bash

# Build script for all SPDK CSI Driver images
# Usage: ./scripts/build-all.sh [REGISTRY] [TAG]

set -e

REGISTRY=${1:-"flint"}
TAG=${2:-"latest"}

echo "Building all SPDK CSI Driver images with registry: $REGISTRY, tag: $TAG"

# Build CSI driver images
echo "=== Building CSI Driver Images ==="

echo "Building Controller..."
docker build -t "${REGISTRY}/flint-controller:${TAG}" \
  -f docker/Dockerfile.controller .

echo "Building CSI Driver..."
docker build -t "${REGISTRY}/flint-driver:${TAG}" \
  -f docker/Dockerfile.csi .

echo "Building Node Agent..."
docker build -t "${REGISTRY}/flint-node-agent:${TAG}" \
  -f docker/Dockerfile.node-agent .

echo "Building SPDK Target..."
docker build -t "${REGISTRY}/spdk-tgt:${TAG}" \
  -f docker/Dockerfile.spdk .

# Build dashboard images
echo "=== Building Dashboard Images ==="

echo "Building Dashboard Backend..."
docker build -t "${REGISTRY}/spdk-dashboard-backend:${TAG}" \
  -f docker/Dockerfile.dashboard-backend .

echo "Building Dashboard Frontend..."
cd ../spdk-dashboard
docker build -t "${REGISTRY}/spdk-dashboard-frontend:${TAG}" \
  -f Dockerfile.frontend .
cd ../spdk-csi-driver

echo ""
echo "🎉 All images built successfully!"
echo ""
echo "CSI Driver Images:"
echo "  Controller:  ${REGISTRY}/flint-controller:${TAG}"
echo "  Driver:      ${REGISTRY}/flint-driver:${TAG}"
echo "  Node Agent:  ${REGISTRY}/flint-node-agent:${TAG}"
echo "  SPDK Target: ${REGISTRY}/spdk-tgt:${TAG}"
echo ""
echo "Dashboard Images:"
echo "  Backend:     ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "  Frontend:    ${REGISTRY}/spdk-dashboard-frontend:${TAG}"
echo ""
echo "To push all images to registry:"
echo "docker push ${REGISTRY}/flint-controller:${TAG}"
echo "docker push ${REGISTRY}/flint-driver:${TAG}"
echo "docker push ${REGISTRY}/flint-node-agent:${TAG}"
echo "docker push ${REGISTRY}/spdk-tgt:${TAG}"
echo "docker push ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "docker push ${REGISTRY}/spdk-dashboard-frontend:${TAG}" 