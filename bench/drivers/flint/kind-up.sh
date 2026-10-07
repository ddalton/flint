#!/usr/bin/env bash
# kind-up.sh -- Phase 0 only: bring up a RELEASED Flint (published driver
# image, chart and rig from the release tag) on a 3-node kind cluster via
# tests/system/kind-spdk.sh, without its test suites (SUITES=" ": the rig
# reads ${SUITES:-all}, so an EMPTY value runs every suite), and apply the bench
# StorageClasses. Numbers from kind are NOT results (one kernel, loopback
# network, all nodes' disks on one NVMe, SPDK's kind-mode small pools);
# this validates the harness.
#
#   RELEASE_TREE=~/bench/flint-v1.58.0 VERSION=1.58.0 DISK=/dev/nvme1n1 \
#     CONFIRM_WIPE=<serial> KUBECONFIG_PRIVATE=~/bench/kubeconfig ./kind-up.sh
#
# DISK is WIPED (see kind-spdk.sh).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
: "${RELEASE_TREE:?checkout of the release tag}" "${VERSION:?e.g. 1.58.0}"
: "${DISK:?}" "${CONFIRM_WIPE:?}" "${KUBECONFIG_PRIVATE:?}"
REG_PORT="${REG_PORT:-5002}"
SPDK_TAG=$(awk '/^  spdkTarget:/{f=1} f&&/tag:/{gsub(/"/,"",$2); print $2; exit}' \
  "$RELEASE_TREE/flint-csi-driver-chart/values.yaml")
drv="localhost:${REG_PORT}/dilipdalton/flint-driver:${VERSION}"
spdk="localhost:${REG_PORT}/dilipdalton/spdk-tgt-kind:${VERSION}"

docker pull "dilipdalton/flint-driver:${VERSION}"
docker tag "dilipdalton/flint-driver:${VERSION}" "$drv"
docker build -f "$RELEASE_TREE/spdk-csi-driver/docker/Dockerfile.spdk_kind" \
  --build-arg BASE_IMAGE="dilipdalton/spdk-tgt:${SPDK_TAG}" -t "$spdk" \
  "$RELEASE_TREE/spdk-csi-driver/docker"

mkdir -p "$(dirname "$KUBECONFIG_PRIVATE")"
BUILD=0 TAG="$VERSION" SUITES=" " KEEP=1 KUBECONFIG_PRIVATE="$KUBECONFIG_PRIVATE" \
  REG_PORT="$REG_PORT" bash "$RELEASE_TREE/tests/system/kind-spdk.sh"
KUBECONFIG="$KUBECONFIG_PRIVATE" kubectl apply -f "$HERE/storageclasses.yaml"
