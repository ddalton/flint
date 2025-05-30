#!/bin/bash
set -e

# Configuration
REGISTRY=${REGISTRY:-"docker-sandbox.infra.cloudera.com/ddalton"}
PROJECT=${PROJECT:-"spdk-csi"}
VERSION=${VERSION:-"v0.4.0"}

echo "Building SPDK CSI Driver ${VERSION}"

# Build Rust binaries
echo "Building Rust components..."
cargo build --release

# Build Docker images
echo "Building Docker images..."

# CSI Driver image
docker build -f docker/Dockerfile.csi -t ${REGISTRY}/${PROJECT}/csi-driver:${VERSION} .

# Controller image  
docker build -f docker/Dockerfile.controller -t ${REGISTRY}/${PROJECT}/controller:${VERSION} .

# Node Agent image
docker build -f docker/Dockerfile.node-agent -t ${REGISTRY}/${PROJECT}/node-agent:${VERSION} .

# SPDK image (if custom SPDK build needed)
docker build -f docker/Dockerfile.spdk -t ${REGISTRY}/${PROJECT}/spdk:${VERSION} .

# Push images
echo "Pushing images..."
docker push ${REGISTRY}/${PROJECT}/csi-driver:${VERSION}
docker push ${REGISTRY}/${PROJECT}/controller:${VERSION}
docker push ${REGISTRY}/${PROJECT}/node-agent:${VERSION}
docker push ${REGISTRY}/${PROJECT}/spdk:${VERSION} 

echo "Build complete!"
echo "CSI Driver: ${REGISTRY}/${PROJECT}/csi-driver:${VERSION}"
echo "Controller: ${REGISTRY}/${PROJECT}/controller:${VERSION}"
echo "Node Agent: ${REGISTRY}/${PROJECT}/node-agent:${VERSION}"
echo "SPDK: ${REGISTRY}/${PROJECT}/spdk:${VERSION}"
