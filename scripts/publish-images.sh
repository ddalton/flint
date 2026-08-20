#!/bin/bash
# ---------------------------------------------------------------------------
# Publish the three flint images multi-arch, from binaries already staged
# by scripts/stage-prebuilt.sh.
#
#   scripts/publish-images.sh 1.31.0 [--dry-run]
#
# WHY NOT release.sh images
#
# `release.sh images` builds amd64-only with buildx and is not the path
# any recent release used. The path that works, and the one this scripts,
# is: cross-compile both arches on the Mac, build a per-arch image from
# the prebuilt binaries, push both, then join them with a manifest list.
#
# MOVING TAGS ARE THE EASY THING TO FORGET. 1.29.0 published its version
# tags and never advanced `latest` or `1`, so both sat at 1.28.0 until
# 1.30.0 jumped them two releases. They are published here, in the same
# loop, from the same digests.
# ---------------------------------------------------------------------------
set -euo pipefail

ver=${1:?usage: publish-images.sh <version> [--dry-run]}
dry=${2:-}
run() { if [ "$dry" = "--dry-run" ]; then echo "  + $*"; else "$@"; fi; }

here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../spdk-csi-driver" && pwd)
cd "$crate"

[ -d docker/prebuilt/amd64 ] && [ -d docker/prebuilt/arm64 ] || {
    echo "docker/prebuilt is not staged — run scripts/stage-prebuilt.sh first" >&2
    exit 1; }

# image:dockerfile
set -- \
    "flint-driver:docker/Dockerfile.csi.prebuilt" \
    "flint-pnfs:docker/Dockerfile.pnfs.prebuilt" \
    "flint-lite-operator:docker/Dockerfile.operator.prebuilt"

minor=${ver%.*}      # 1.31.0 -> 1.31
major=${ver%%.*}     # 1.31.0 -> 1

for spec in "$@"; do
    name=${spec%%:*}
    dockerfile=${spec#*:}
    repo="dilipdalton/$name"
    echo "=== $repo ==="

    for arch in amd64 arm64; do
        echo "--- build $arch ---"
        run docker build --platform "linux/$arch" \
            -f "$dockerfile" --build-arg BIN_DIR=docker/prebuilt \
            -t "$repo:$ver-$arch" .
        run docker push "$repo:$ver-$arch"
    done

    # One manifest list per tag, all from the same two digests. `--amend`
    # so a re-run replaces rather than appending a duplicate.
    for tag in "$ver" "$minor" "$major" latest; do
        echo "--- manifest $repo:$tag ---"
        run docker manifest rm "$repo:$tag" 2>/dev/null || true
        run docker manifest create "$repo:$tag" \
            --amend "$repo:$ver-amd64" --amend "$repo:$ver-arm64"
        run docker manifest push "$repo:$tag"
    done
done

echo
echo "published $ver (+ $minor, $major, latest) for all three images"
