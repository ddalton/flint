#!/usr/bin/env bash
# Build the four s3.chert.us images for the LOCAL kind rig and load them.
#
#   ./build-images.sh [kind-cluster-name]     (default: flint-s3csi)
#
# Cross-builds on the Mac (cargo zigbuild → aarch64 or x86_64 musl,
# matching the kind node's arch), stages under docker/prebuilt/<arch>/,
# builds single-arch images with buildx --load, and `kind load`s them.
# Release publishing goes through scripts/stage-prebuilt.sh +
# scripts/publish-images.sh with the `s3csi` scope, not this file.
set -euo pipefail
cd "$(dirname "$0")/../.."
CLUSTER=${1:-flint-s3csi}
TAG=${TAG:-dev}

case "$(docker info --format '{{.Architecture}}' 2>/dev/null)" in
    aarch64|arm64) ARCH=arm64; TRIPLE=aarch64-unknown-linux-musl ;;
    x86_64|amd64)  ARCH=amd64; TRIPLE=x86_64-unknown-linux-musl ;;
    *) echo "unknown docker architecture" >&2; exit 2 ;;
esac
echo "building for $ARCH ($TRIPLE), tag $TAG"

( cd spdk-csi-driver && cargo zigbuild --release --target "$TRIPLE" --bin flint-s3-csi-node --bin flint-s3-broker )
( cd crates/flint-s3-worker && cargo zigbuild --release --target "$TRIPLE" )

STAGE=spdk-csi-driver/docker/prebuilt/$ARCH
mkdir -p "$STAGE"
cp spdk-csi-driver/target/$TRIPLE/release/flint-s3-csi-node "$STAGE/"
cp spdk-csi-driver/target/$TRIPLE/release/flint-s3-broker "$STAGE/"
cp crates/flint-s3-worker/target/$TRIPLE/release/flint-s3-worker "$STAGE/"
ls -la "$STAGE"/flint-s3-*

docker buildx build --platform linux/$ARCH --load \
    -f spdk-csi-driver/docker/Dockerfile.s3csi.prebuilt \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-s3-csi:$TAG" .
docker buildx build --platform linux/$ARCH --load \
    -f spdk-csi-driver/docker/Dockerfile.s3worker \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-s3-worker:$TAG" .
# The lean worker: FROM the pinned flint-sync image (SYNC_IMAGE in the
# Dockerfile), plus the same worker binary.
docker buildx build --platform linux/$ARCH --load \
    -f spdk-csi-driver/docker/Dockerfile.s3worker-lean \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-s3-worker-lean:$TAG" .

kind load docker-image --name "$CLUSTER" "dilipdalton/flint-s3-csi:$TAG" "dilipdalton/flint-s3-worker:$TAG" "dilipdalton/flint-s3-worker-lean:$TAG"
echo "loaded into kind cluster $CLUSTER: flint-s3-csi:$TAG flint-s3-worker:$TAG flint-s3-worker-lean:$TAG"
