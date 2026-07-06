#!/bin/bash
# ---------------------------------------------------------------------------
# Release gate for the flint-csi-driver chart and its images.
#
#   scripts/release.sh check    # verify every image tag the chart references
#                               # exists on Docker Hub (default)
#   scripts/release.sh images   # build + push the MISSING images only
#   scripts/release.sh chart    # verify, then helm package + push the chart
#   scripts/release.sh all      # images, then chart
#
# Why this exists: the chart is the source of truth for image tags
# (flint-csi-driver-chart/values.yaml), but releases were pushed by hand —
# 1.2.0 shipped with flint-driver:1.2.0 published while
# spdk-dashboard-frontend:1.2.0 was never pushed, so every install sat in
# ImagePullBackOff. This script derives the required image list FROM
# values.yaml, so the chart cannot be pushed while one of its image
# references is unpublished.
#
# Build notes:
#   - Published tags are never rebuilt (tags are immutable by convention,
#     and the SPDK image takes ~an hour even on a native amd64 node).
#   - Images build for linux/amd64. On an Apple Silicon Mac the SPDK and
#     driver builds run under QEMU and take hours — point DOCKER_HOST at a
#     native amd64 daemon instead, see
#     spdk-csi-driver/docs/remote-x86-build-node.md. The frontend image is
#     a Node build and is fine anywhere.
#   - Pushing the chart needs helm registry auth for registry-1.docker.io
#     (helm reuses the Docker login; otherwise `helm registry login`).
#   - kindMode's spdk-tgt-kind:latest is a dev-only image (kindMode is
#     disabled by default) and is deliberately not gated here.
#
# Kept bash-3.2 compatible (macOS /bin/bash): no mapfile, no declare -A.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo_root=$(cd "$here/.." && pwd)
chart_dir="$repo_root/flint-csi-driver-chart"

cmd=${1:-check}
case "$cmd" in check|images|chart|all) ;; *)
    echo "usage: $0 [check|images|chart|all]" >&2; exit 2 ;;
esac

# --- read the chart ---------------------------------------------------------
# images_table lines:  <name> <tag> <context-dir> <dockerfile-rel-to-context>
# The build recipes live here too so values.yaml stays the single source of
# truth for WHAT ships and this table only adds HOW it builds.
read -r hub_ns chart_version app_version <<EOF
$(python3 - "$chart_dir" <<'PYEOF'
import sys, yaml
d = sys.argv[1]
values = yaml.safe_load(open(f"{d}/values.yaml"))
chart = yaml.safe_load(open(f"{d}/Chart.yaml"))
print(values["images"]["repository"], chart["version"], chart["appVersion"])
PYEOF
)
EOF

images_table=$(python3 - "$chart_dir" <<'PYEOF'
import sys, yaml
d = sys.argv[1]
values = yaml.safe_load(open(f"{d}/values.yaml"))
images = values["images"]
print(images["flintCsiDriver"]["name"], images["flintCsiDriver"]["tag"],
      "spdk-csi-driver", "docker/Dockerfile.csi")
print(images["spdkTarget"]["name"], images["spdkTarget"]["tag"],
      "spdk-csi-driver", "docker/Dockerfile.spdk")
dash = values.get("dashboard", {})
if dash.get("enabled", False):
    print(dash["frontend"]["name"], dash["frontend"]["tag"],
          "spdk-dashboard", "Dockerfile.frontend")
# pNFS server image (MDS+DS, one image). Gated whenever a tag is set,
# even though pnfs.server is disabled by default: anyone flipping it
# on must find the image published — the exact 1.2.0-frontend class
# of bug this script exists to prevent.
pnfs = values.get("pnfs", {}).get("server", {})
if pnfs.get("image", {}).get("tag"):
    print(pnfs["image"]["name"], pnfs["image"]["tag"],
          "spdk-csi-driver", "docker/Dockerfile.pnfs")
PYEOF
)

tag_exists() {  # <name> <tag> -> 0 if published on Docker Hub
    curl -fsS -o /dev/null \
        "https://hub.docker.com/v2/repositories/$hub_ns/$1/tags/$2" 2>/dev/null
}

# --- check ------------------------------------------------------------------
echo "chart $chart_version (appVersion $app_version) references:"
missing_table=""
while read -r name tag ctx file; do
    [ -n "$name" ] || continue
    if tag_exists "$name" "$tag"; then
        echo "  ✓ $hub_ns/$name:$tag"
    else
        echo "  ✗ $hub_ns/$name:$tag  — NOT on Docker Hub"
        missing_table="$missing_table$name $tag $ctx $file
"
    fi
done <<EOF
$images_table
EOF

if [ "$cmd" = check ]; then
    [ -z "$missing_table" ] && exit 0
    echo "missing image(s): run '$0 images' to build and push them." >&2
    exit 1
fi

# --- images: build + push only what's missing --------------------------------
if [ "$cmd" = images ] || [ "$cmd" = all ]; then
    if [ -z "$missing_table" ]; then
        echo "all referenced images are published; nothing to build."
    fi
    while read -r name tag ctx file; do
        [ -n "$name" ] || continue
        ref="$hub_ns/$name:$tag"
        echo "── building $ref"
        echo "   context $repo_root/$ctx"
        docker buildx build --platform linux/amd64 \
            -f "$repo_root/$ctx/$file" -t "$ref" --push "$repo_root/$ctx"
        echo "   pushed $ref"
    done <<EOF
$missing_table
EOF
fi

# --- chart: verify everything again, then package + push ---------------------
if [ "$cmd" = chart ] || [ "$cmd" = all ]; then
    while read -r name tag ctx file; do
        [ -n "$name" ] || continue
        if ! tag_exists "$name" "$tag"; then
            echo "REFUSING to push chart $chart_version:" \
                 "$hub_ns/$name:$tag is not on Docker Hub." >&2
            exit 1
        fi
    done <<EOF
$images_table
EOF
    pkg_dir=$(mktemp -d)
    trap 'rm -rf "$pkg_dir"' EXIT
    helm package "$chart_dir" --destination "$pkg_dir" >/dev/null
    pkg="$pkg_dir/flint-csi-driver-chart-$chart_version.tgz"
    echo "── pushing $(basename "$pkg") to oci://registry-1.docker.io/$hub_ns"
    helm push "$pkg" "oci://registry-1.docker.io/$hub_ns"
    echo "chart $chart_version released."
fi
