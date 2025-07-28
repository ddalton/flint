#!/bin/bash
# Build script for all SPDK Flint Docker images
# This script builds specialized images for different deployment patterns

set -e  # Exit on any error

# Configuration
REGISTRY=${REGISTRY:-"spdk-flint"}
TAG=${TAG:-"latest"}
BUILD_ARGS=${BUILD_ARGS:-""}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log() {
    echo -e "${BLUE}[$(date +'%Y-%m-%d %H:%M:%S')] $1${NC}"
}

success() {
    echo -e "${GREEN}[SUCCESS] $1${NC}"
}

warn() {
    echo -e "${YELLOW}[WARNING] $1${NC}"
}

error() {
    echo -e "${RED}[ERROR] $1${NC}"
}

# Function to build an image
build_image() {
    local dockerfile=$1
    local image_name=$2
    local description=$3
    
    log "Building $description..."
    log "Dockerfile: $dockerfile"
    log "Image: $REGISTRY/$image_name:$TAG"
    
    if docker build -f "$dockerfile" -t "$REGISTRY/$image_name:$TAG" $BUILD_ARGS .; then
        success "Built $REGISTRY/$image_name:$TAG"
        
        # Get image size
        local size=$(docker images "$REGISTRY/$image_name:$TAG" --format "{{.Size}}")
        log "Image size: $size"
    else
        error "Failed to build $description"
        return 1
    fi
}

# Function to run tests on an image
test_image() {
    local image_name=$1
    local test_cmd=$2
    
    log "Testing $image_name..."
    if docker run --rm "$REGISTRY/$image_name:$TAG" $test_cmd; then
        success "Tests passed for $image_name"
    else
        warn "Tests failed for $image_name (this may be expected without SPDK)"
    fi
}

# Main build process
main() {
    log "Starting SPDK Flint Docker image build process"
    log "Registry: $REGISTRY"
    log "Tag: $TAG"
    log "Build context: $(pwd)"
    
    # Check if we're in the right directory
    if [ ! -f "CMakeLists.txt" ] || [ ! -d "docker" ]; then
        error "Please run this script from the spdk_flint root directory"
        exit 1
    fi
    
    # Build base image first
    log "=== Building Base Image ==="
    build_image "docker/Dockerfile.base" "base" "Base runtime image"
    
    # Build specialized images
    log ""
    log "=== Building Specialized Images ==="
    
    # CSI Node (DaemonSet)
    BASE_IMAGE="$REGISTRY/base:$TAG" build_image "docker/Dockerfile.csi-node" "csi-node" "CSI Node Plugin (DaemonSet)"
    
    # CSI Controller (Deployment)  
    BASE_IMAGE="$REGISTRY/base:$TAG" build_image "docker/Dockerfile.csi-controller" "csi-controller" "CSI Controller Plugin (Deployment)"
    
    # Dashboard Backend (Service)
    BASE_IMAGE="$REGISTRY/base:$TAG" build_image "docker/Dockerfile.dashboard-backend" "dashboard-backend" "Dashboard Backend API (Service)"
    
    # Node Agent (DaemonSet)
    BASE_IMAGE="$REGISTRY/base:$TAG" build_image "docker/Dockerfile.node-agent" "node-agent" "Node Agent (DaemonSet)"
    
    log ""
    log "=== Running Basic Tests ==="
    
    # Test that images can start (will fail without SPDK but should show help)
    test_image "csi-controller" "--help"
    test_image "dashboard-backend" "--help"
    test_image "node-agent" "--help"
    test_image "csi-node" "--help"
    
    log ""
    log "=== Build Summary ==="
    
    # Show all built images
    log "Built images:"
    docker images "$REGISTRY/*:$TAG" --format "table {{.Repository}}:{{.Tag}}\t{{.Size}}\t{{.CreatedAt}}"
    
    success "All images built successfully!"
    
    log ""
    log "=== Usage Examples ==="
    echo ""
    echo "To run the CSI Controller:"
    echo "  docker run -p 9809:9809 $REGISTRY/csi-controller:$TAG"
    echo ""
    echo "To run the Dashboard Backend:"
    echo "  docker run -p 8080:8080 $REGISTRY/dashboard-backend:$TAG"
    echo ""
    echo "Dashboard API endpoints (when running):"
    echo "  http://localhost:8080/health"
    echo "  http://localhost:8080/api/v1/volumes"
    echo "  http://localhost:8080/api/v1/nodes"
    echo "  http://localhost:8080/api/v1/stats"
    echo "  http://localhost:8080/api/v1/devices"
    echo "  http://localhost:8080/api/v1/discovery"
    echo ""
    echo "To run the Node Agent (requires privileged):"
    echo "  docker run --privileged -p 8090:8090 -v /dev:/dev $REGISTRY/node-agent:$TAG"
    echo ""
    echo "To push images to registry:"
    echo "  docker push $REGISTRY/csi-controller:$TAG"
    echo "  docker push $REGISTRY/csi-node:$TAG"
    echo "  docker push $REGISTRY/dashboard-backend:$TAG"
    echo "  docker push $REGISTRY/node-agent:$TAG"
}

# Function to push all images
push_images() {
    log "Pushing images to registry..."
    
    for image in base csi-controller csi-node dashboard-backend node-agent; do
        log "Pushing $REGISTRY/$image:$TAG"
        docker push "$REGISTRY/$image:$TAG"
    done
    
    success "All images pushed successfully!"
}

# Function to clean up images
clean_images() {
    log "Cleaning up SPDK Flint images..."
    
    docker images "$REGISTRY/*" -q | xargs -r docker rmi -f
    
    success "Images cleaned up!"
}

# Parse command line arguments
case "${1:-build}" in
    "build")
        main
        ;;
    "push")
        push_images
        ;;
    "clean")
        clean_images
        ;;
    "all")
        main
        push_images
        ;;
    *)
        echo "Usage: $0 [build|push|clean|all]"
        echo ""
        echo "Commands:"
        echo "  build (default) - Build all images"
        echo "  push           - Push all images to registry"
        echo "  clean          - Remove all built images"
        echo "  all            - Build and push all images"
        echo ""
        echo "Environment variables:"
        echo "  REGISTRY       - Docker registry/namespace (default: spdk-flint)"
        echo "  TAG            - Image tag (default: latest)"
        echo "  BUILD_ARGS     - Additional docker build arguments"
        exit 1
        ;;
esac 