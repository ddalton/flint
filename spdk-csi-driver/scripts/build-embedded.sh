#!/bin/bash

# Build script for SPDK CSI Driver with Embedded SPDK
# Usage: ./scripts/build-embedded.sh [REGISTRY] [TAG]

set -e

REGISTRY=${1:-"flint"}
TAG=${2:-"latest"}

echo "Building SPDK CSI Driver with Embedded SPDK - registry: $REGISTRY, tag: $TAG"

# Build CSI driver images
echo "=== Building CSI Driver Images (Embedded Architecture) ==="

echo "Building Controller..."
docker build -t "${REGISTRY}/flint-controller:${TAG}" \
  -f docker/Dockerfile.controller .

echo "Building CSI Driver..."
docker build -t "${REGISTRY}/flint-driver:${TAG}" \
  -f docker/Dockerfile.csi .

echo "Building Node Agent with Embedded SPDK..."
docker build -t "${REGISTRY}/flint-node-agent-embedded:${TAG}" \
  -f docker/Dockerfile.node-agent-embedded .

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
echo "🎉 All embedded images built successfully!"
echo ""
echo "CSI Driver Images (Embedded Architecture):"
echo "  Controller:             ${REGISTRY}/flint-controller:${TAG}"
echo "  Driver:                 ${REGISTRY}/flint-driver:${TAG}"
echo "  Node Agent (Embedded):  ${REGISTRY}/flint-node-agent-embedded:${TAG}"
echo ""
echo "Dashboard Images:"
echo "  Backend:                ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "  Frontend:               ${REGISTRY}/spdk-dashboard-frontend:${TAG}"
echo ""
echo "Architecture Benefits:"
echo "  ✅ Simplified deployment (4 containers vs 5)"
echo "  ✅ Better performance (direct SPDK API calls)"
echo "  ✅ Reduced resource overhead"
echo "  ✅ Easier troubleshooting"
echo ""
echo "To push all images to registry:"
echo "docker push ${REGISTRY}/flint-controller:${TAG}"
echo "docker push ${REGISTRY}/flint-driver:${TAG}" 
echo "docker push ${REGISTRY}/flint-node-agent-embedded:${TAG}"
echo "docker push ${REGISTRY}/spdk-dashboard-backend:${TAG}"
echo "docker push ${REGISTRY}/spdk-dashboard-frontend:${TAG}" 