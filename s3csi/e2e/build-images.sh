#!/usr/bin/env bash
# Build the four s3.csi.chert.us images for the LOCAL kind rig and load them.
#
#   ./build-images.sh [kind-cluster-name]     (default: flint-s3csi)
#   PUSH=1 ARCH=amd64 TAG=<tag> ./build-images.sh    # for a real cluster
#
# Cross-builds on the Mac (cargo zigbuild → aarch64 or x86_64 musl,
# matching the kind node's arch, or ARCH= to say), stages under
# docker/prebuilt/<arch>/, builds single-arch images with buildx --load,
# and `kind load`s them. With PUSH=1 the images are pushed to the
# registry under TAG instead — the drill on a cluster whose nodes pull
# from Docker Hub (run-s3csi.sh STORE=s3 NODE_EXEC=nodesh). Release
# publishing goes through scripts/stage-prebuilt.sh +
# scripts/publish-images.sh with the `s3csi` scope, not this file.
set -euo pipefail
cd "$(dirname "$0")/../.."
CLUSTER=${1:-flint-s3csi}
TAG=${TAG:-dev}
PUSH=${PUSH:-0}

case "${ARCH:-$(docker info --format '{{.Architecture}}' 2>/dev/null)}" in
    aarch64|arm64) ARCH=arm64; TRIPLE=aarch64-unknown-linux-musl ;;
    x86_64|amd64)  ARCH=amd64; TRIPLE=x86_64-unknown-linux-musl ;;
    *) echo "unknown docker architecture" >&2; exit 2 ;;
esac
if [ "$PUSH" = 1 ]; then OUT=--push; else OUT=--load; fi
echo "building for $ARCH ($TRIPLE), tag $TAG ($OUT)"

( cd spdk-csi-driver && cargo zigbuild --release --target "$TRIPLE" --bin flint-s3-csi-node --bin flint-s3-broker )
( cd crates/flint-s3-worker && cargo zigbuild --release --target "$TRIPLE" )
# flint-sync from THIS checkout. Without it the lean worker image is
# built FROM a pinned published flint-sync (Dockerfile.s3worker-lean's
# SYNC_IMAGE default), so every lean leg — S11 to S14 — measures a
# RELEASED syncer and no change to lean/sidecar is visible to the drill
# at all. That was true until 2026-09-03 and is exactly the kind of
# silent staleness the stage-prebuilt guard exists to prevent elsewhere.
( cd lean/sidecar && cargo zigbuild --release --target "$TRIPLE" --features s3 --bin flint-sync )

STAGE=spdk-csi-driver/docker/prebuilt/$ARCH
mkdir -p "$STAGE"
cp spdk-csi-driver/target/$TRIPLE/release/flint-s3-csi-node "$STAGE/"
cp spdk-csi-driver/target/$TRIPLE/release/flint-s3-broker "$STAGE/"
cp crates/flint-s3-worker/target/$TRIPLE/release/flint-s3-worker "$STAGE/"
cp lean/sidecar/target/$TRIPLE/release/flint-sync "$STAGE/"
ls -la "$STAGE"/flint-s3-*

docker buildx build --platform linux/$ARCH $OUT \
    -f spdk-csi-driver/docker/Dockerfile.s3csi.prebuilt \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-s3-csi:$TAG" .
docker buildx build --platform linux/$ARCH $OUT \
    -f spdk-csi-driver/docker/Dockerfile.s3worker \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-s3-worker:$TAG" .
docker buildx build --platform linux/$ARCH $OUT \
    -f spdk-csi-driver/docker/Dockerfile.sync.prebuilt \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    -t "dilipdalton/flint-sync:$TAG" .
if [ "$PUSH" = 1 ]; then
    # Pushed: buildx resolves the FROM against the registry, where
    # flint-sync:$TAG now is — the case the plain-build note below is
    # NOT about.
    docker buildx build --platform linux/$ARCH --push \
        -f spdk-csi-driver/docker/Dockerfile.s3worker-lean \
        --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
        --build-arg SYNC_IMAGE="dilipdalton/flint-sync:$TAG" \
        -t "dilipdalton/flint-s3-worker-lean:$TAG" .
    echo "pushed: dilipdalton/{flint-s3-csi,flint-s3-worker,flint-sync,flint-s3-worker-lean}:$TAG ($ARCH)"
    exit 0
fi
# The lean worker: FROM the flint-sync image just built from THIS
# checkout (not the Dockerfile's published default), plus the worker.
#
# PLAIN `docker build` ON PURPOSE, not buildx. The repo's
# `flint-multiarch` builder uses the docker-container driver, which
# resolves a `FROM` against the REGISTRY and cannot see an image
# `--load`ed into the local daemon a moment earlier — it fails with a
# bare "not found" that reads like a typo. `--builder default` is not the
# escape hatch either: it belongs to the `default` docker CONTEXT and
# errors out under any other one. The daemon's own builder resolves
# against the daemon, which is where flint-sync:$TAG just landed.
docker build --platform linux/$ARCH \
    -f spdk-csi-driver/docker/Dockerfile.s3worker-lean \
    --build-arg BIN_DIR=spdk-csi-driver/docker/prebuilt \
    --build-arg SYNC_IMAGE="dilipdalton/flint-sync:$TAG" \
    -t "dilipdalton/flint-s3-worker-lean:$TAG" .

kind load docker-image --name "$CLUSTER" "dilipdalton/flint-s3-csi:$TAG" "dilipdalton/flint-s3-worker:$TAG" "dilipdalton/flint-s3-worker-lean:$TAG" "dilipdalton/flint-sync:$TAG"
echo "loaded into kind cluster $CLUSTER: flint-s3-csi:$TAG flint-s3-worker:$TAG flint-s3-worker-lean:$TAG flint-sync:$TAG"
