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

ver=${1:?usage: publish-images.sh <version> [all|lean] [--dry-run]}
# Scope, matching stage-prebuilt.sh. A lean-scoped release publishes the
# operator image (which carries the lean binaries) and the sidecar, then
# aliases the operator to its lean name — it does not republish the CSI
# driver or the pNFS image, because nothing in them changed.
SCOPE=all
for a in "$@"; do case "$a" in lean) SCOPE=lean ;; esac; done
dry=${2:-}
run() { if [ "$dry" = "--dry-run" ]; then echo "  + $*"; else "$@"; fi; }

here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../spdk-csi-driver" && pwd)
cd "$crate"

[ -d docker/prebuilt/amd64 ] && [ -d docker/prebuilt/arm64 ] || {
    echo "docker/prebuilt is not staged — run scripts/stage-prebuilt.sh first" >&2
    exit 1; }

# image:dockerfile
#
# `flint-sync` joined this list on 2026-08-26. Until then the image the
# lean webhook INJECTS INTO EVERY WORKSPACE POD was published by hand,
# following a recipe written in a comment at the top of its own
# Dockerfile — no staging check, no staleness check, no multi-arch
# manifest step that anything verified. `release.sh` only asks whether a
# tag EXISTS on the Hub, which a hand-pushed wrong build satisfies
# perfectly. An unpublished or stale sidecar is a fleet of pods that
# never start, with the operator itself perfectly healthy.
set -- \
    "flint-driver:docker/Dockerfile.csi.prebuilt" \
    "flint-pnfs:docker/Dockerfile.pnfs.prebuilt" \
    "flint-lite-operator:docker/Dockerfile.operator.prebuilt" \
    "flint-sync:docker/Dockerfile.sync.prebuilt"

if [ "$SCOPE" = lean ]; then
    set -- \
        "flint-lite-operator:docker/Dockerfile.operator.prebuilt" \
        "flint-sync:docker/Dockerfile.sync.prebuilt"
fi

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

# --- aliases: one image, two names ------------------------------------------
# flint-lean installs out of the SAME image as flint-lite-operator (same
# crate, same build; the chart picks the binary), but asking someone to
# pull "flint-lite-operator" to install flint-lean reads as a dependency
# it does not have. So the identical manifest list is republished under
# a lean-shaped name.
#
# imagetools create COPIES the index cross-repo (blobs are mounted, not
# re-uploaded) and preserves the digest, so the alias is provably the
# same bits — release.sh gates on exactly that equality. Do NOT build
# here: a rebuild would produce a different digest and the two names
# would drift, which is the whole failure this is meant to avoid.
#
# NOTE the alias name is flint-lean-OPERATOR, not flint-lean:
# dilipdalton/flint-lean is the OCI repo the Helm chart is pushed to,
# and mixing a chart artifact and a container image in one repo makes
# `docker pull` and `helm pull` disagree about what a tag means.
set -- "flint-lite-operator:flint-lean-operator"

for spec in "$@"; do
    from=${spec%%:*}
    to=${spec#*:}
    echo "=== dilipdalton/$to (alias of $from) ==="
    for tag in "$ver" "$minor" "$major" latest; do
        echo "--- alias $to:$tag ---"
        run docker buildx imagetools create \
            -t "dilipdalton/$to:$tag" "dilipdalton/$from:$ver"
    done
done

echo
echo "published $ver (+ $minor, $major, latest) for all three images"
echo "aliased   flint-lite-operator -> flint-lean-operator at the same digest"
