#!/usr/bin/env bash
# Build the three flint-forge images from THIS checkout for the local
# kind rig and load them — the forge analogue of
# s3csi/e2e/build-images.sh. The published images are multi-arch and
# would work, but a drill that verifies a claim scored from the current
# code must run the current code, not a release.
#
#   ./forge/e2e/build-forge-images.sh [kind-cluster-name]   (default: flint-s3csi)
#   TAG=<tag> ...                                            (default: dev)
#   ARCH=amd64 PUSH=1 TAG=drill-<sha7> ...    push to Docker Hub for a real
#                                             cluster instead of kind-loading
#
# Cross-builds five binaries from three crates on the Mac, stages them
# under spdk-csi-driver/docker/prebuilt/<arch>/ the way the release
# path expects, builds single-arch images with `docker build --load`
# (context = spdk-csi-driver, which is where the forge Dockerfiles' COPY
# paths resolve), and `kind load`s them.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
CLUSTER=${1:-flint-s3csi}
TAG=${TAG:-dev}
PUSH=${PUSH:-0}
# Stamped into every image so a running pod can be tied back to a
# commit by content (`docker inspect`, or the imageID in the pod
# status), which a tag cannot do: this repository has shipped two
# images whose tag said one version and whose binaries were another.
REV=$(git rev-parse HEAD)

case "${ARCH:-$(docker info --format '{{.Architecture}}' 2>/dev/null)}" in
    aarch64|arm64) ARCH=arm64; TRIPLE=aarch64-unknown-linux-musl ;;
    x86_64|amd64)  ARCH=amd64; TRIPLE=x86_64-unknown-linux-musl ;;
    *) echo "unknown docker architecture" >&2; exit 2 ;;
esac
echo "building forge for $ARCH ($TRIPLE), tag $TAG"

( cd spdk-csi-driver && cargo zigbuild --release --target "$TRIPLE" --bin flint-forge-operator --bin flint-hub-gateway )
# The syncer bin has `required-features = ["s3"]` (it talks to the
# bucket). It is also the hook: the git image installs the same binary
# under the two hook names.
( cd forge/syncer   && cargo zigbuild --release --target "$TRIPLE" --features s3 --bin flint-forge-syncer )
( cd lean/sidecar   && cargo zigbuild --release --target "$TRIPLE" --features s3 --bin flint-sync )

STAGE=spdk-csi-driver/docker/prebuilt/$ARCH
mkdir -p "$STAGE"
cp spdk-csi-driver/target/$TRIPLE/release/flint-forge-operator "$STAGE/"
cp spdk-csi-driver/target/$TRIPLE/release/flint-hub-gateway    "$STAGE/"
cp forge/syncer/target/$TRIPLE/release/flint-forge-syncer      "$STAGE/"
cp lean/sidecar/target/$TRIPLE/release/flint-sync             "$STAGE/"
ls -la "$STAGE"/flint-forge-* "$STAGE"/flint-hub-gateway "$STAGE"/flint-sync

# Context is spdk-csi-driver: the forge-git Dockerfile COPYs
# docker/forge/nginx.conf and docker/prebuilt/<arch>/… , both relative
# to that directory. BIN_DIR=docker/prebuilt, matching the release.
build() { # image dockerfile
    docker build --platform "linux/$ARCH" \
        -f "spdk-csi-driver/$2" \
        --build-arg BIN_DIR=docker/prebuilt \
        --label "org.opencontainers.image.revision=$REV" \
        -t "dilipdalton/$1:$TAG" \
        spdk-csi-driver
}
build flint-forge-operator docker/Dockerfile.forge-operator.prebuilt
build flint-forge-syncer   docker/Dockerfile.forge-syncer.prebuilt
build flint-forge-git      docker/Dockerfile.forge-git

if [ "$PUSH" = 1 ]; then
    for i in operator syncer git; do
        docker push "dilipdalton/flint-forge-$i:$TAG"
        printf '%s  %s\n' "$(docker inspect --format '{{index .RepoDigests 0}}' "dilipdalton/flint-forge-$i:$TAG")" "rev $REV"
    done
    echo "pushed flint-forge-{operator,syncer,git}:$TAG ($ARCH, revision $REV)"
else
    kind load docker-image --name "$CLUSTER" \
        "dilipdalton/flint-forge-operator:$TAG" \
        "dilipdalton/flint-forge-syncer:$TAG" \
        "dilipdalton/flint-forge-git:$TAG"
    echo "loaded into kind cluster $CLUSTER: flint-forge-{operator,syncer,git}:$TAG"
fi
