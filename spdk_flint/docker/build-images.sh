#!/bin/bash
set -e

# SPDK Flint Single Image Build Script
# This script builds the consolidated image that can run in multiple modes

# Configuration
REGISTRY=${REGISTRY:-"docker-sandbox.infra.cloudera.com/ddalton"}
IMAGE_NAME="flint-base"
VERSION=${VERSION:-"latest"}
BUILD_TYPE=${CMAKE_BUILD_TYPE:-"Release"}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}   SPDK Flint Single Image Builder     ${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

# Print configuration
echo -e "${YELLOW}Configuration:${NC}"
echo -e "  Registry: ${REGISTRY}"
echo -e "  Image: ${IMAGE_NAME}"
echo -e "  Version: ${VERSION}"
echo -e "  Build Type: ${BUILD_TYPE}"
echo -e "  Full Image: ${REGISTRY}/${IMAGE_NAME}:${VERSION}"
echo ""

# Build the consolidated image
echo -e "${BLUE}Building consolidated SPDK Flint image...${NC}"
echo -e "${YELLOW}This single image can run as:${NC}"
echo -e "  • Controller (CSI_MODE=controller)"
echo -e "  • Node Agent (CSI_MODE=node-agent)" 
echo -e "  • Dashboard Backend (CSI_MODE=dashboard-backend)"
echo -e "  • SPDK Driver (CSI_MODE=spdk-driver)"
echo ""

# Change to project root
cd "$(dirname "$0")/.."

# Build the image
echo -e "${GREEN}Building image: ${REGISTRY}/${IMAGE_NAME}:${VERSION}${NC}"
docker build \
    --build-arg CMAKE_BUILD_TYPE="${BUILD_TYPE}" \
    -f docker/Dockerfile.base \
    -t "${REGISTRY}/${IMAGE_NAME}:${VERSION}" \
    .

echo ""
echo -e "${GREEN}✅ Build completed successfully!${NC}"
echo ""

# Show usage examples
echo -e "${BLUE}Usage Examples:${NC}"
echo ""
echo -e "${YELLOW}1. Controller Mode:${NC}"
echo "  docker run -e CSI_MODE=controller ${REGISTRY}/${IMAGE_NAME}:${VERSION}"
echo ""
echo -e "${YELLOW}2. Node Agent Mode:${NC}"
echo "  docker run --privileged -e CSI_MODE=node-agent \\"
echo "    -v /dev:/dev -v /sys:/sys \\"
echo "    ${REGISTRY}/${IMAGE_NAME}:${VERSION}"
echo ""
echo -e "${YELLOW}3. Dashboard Backend Mode:${NC}"
echo "  docker run -e CSI_MODE=dashboard-backend -p 8080:8080 \\"
echo "    ${REGISTRY}/${IMAGE_NAME}:${VERSION}"
echo ""
echo -e "${YELLOW}4. Push to Registry:${NC}"
echo "  docker push ${REGISTRY}/${IMAGE_NAME}:${VERSION}"
echo ""

# Optionally push if requested
if [[ "$1" == "--push" ]]; then
    echo -e "${BLUE}Pushing image to registry...${NC}"
    docker push "${REGISTRY}/${IMAGE_NAME}:${VERSION}"
    echo -e "${GREEN}✅ Image pushed successfully!${NC}"
fi

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}   Build Complete!                     ${NC}"
echo -e "${GREEN}========================================${NC}" 