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

    # The flint-lite chart ships alongside as its OWN OCI artifact
    # (independent version; its appVersion pins the flint-pnfs tag, which
    # the gate above already verified via the pnfs image row).
    lite_dir="$repo_root/flint-lite-chart"
    lite_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$repo_root/flint-lite-chart/Chart.yaml'))['version'])")
    lite_app=$(python3 -c "import yaml; print(yaml.safe_load(open('$repo_root/flint-lite-chart/Chart.yaml'))['appVersion'])")
    if ! tag_exists flint-pnfs "$lite_app"; then
        echo "REFUSING to push flint-lite $lite_version:"              "$hub_ns/flint-pnfs:$lite_app (its appVersion default) is not on Docker Hub." >&2
        exit 1
    fi
    helm package "$lite_dir" --destination "$pkg_dir" >/dev/null
    lite_pkg="$pkg_dir/flint-lite-$lite_version.tgz"
    echo "── pushing $(basename "$lite_pkg") to oci://registry-1.docker.io/$hub_ns"
    helm push "$lite_pkg" "oci://registry-1.docker.io/$hub_ns"
    echo "chart flint-lite $lite_version released."

    # The flint-lite OPERATOR chart, likewise its own artifact. Two
    # images must exist for it to be installable: its own (the
    # operator) and the hub image its appVersion makes the fleet
    # default — a fleet whose default hub image is unpublished is a
    # fleet of ImagePullBackOff, which is the 1.2.0 bug this script
    # exists to prevent, one level up.
    op_dir="$repo_root/flint-lite-operator-chart"
    if [ -d "$op_dir" ]; then
        op_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$op_dir/Chart.yaml'))['version'])")
        op_app=$(python3 -c "import yaml; print(yaml.safe_load(open('$op_dir/Chart.yaml'))['appVersion'])")
        for img in flint-lite-operator flint-pnfs; do
            if ! tag_exists "$img" "$op_app"; then
                echo "REFUSING to push flint-lite-operator $op_version:" \
                     "$hub_ns/$img:$op_app is not on Docker Hub." >&2
                exit 1
            fi
        done
        # The checked-in CRD is install-time bootstrap for the operator's
        # own compiled-in copy; if they disagree, the chart would install
        # a schema the operator immediately replaces (or, with
        # manageCrd: false, one that silently prunes fields).
        gen=$(cd "$repo_root/spdk-csi-driver" && cargo run --quiet --bin crdgen)
        if ! printf '%s\n' "$gen" | diff -q - "$op_dir/crds/flintshares.yaml" >/dev/null; then
            echo "REFUSING to push flint-lite-operator $op_version: crds/flintshares.yaml" \
                 "is stale — regenerate with: cargo run --bin crdgen >" \
                 "flint-lite-operator-chart/crds/flintshares.yaml" >&2
            exit 1
        fi
        helm package "$op_dir" --destination "$pkg_dir" >/dev/null
        op_pkg="$pkg_dir/flint-lite-operator-$op_version.tgz"
        echo "── pushing $(basename "$op_pkg") to oci://registry-1.docker.io/$hub_ns"
        helm push "$op_pkg" "oci://registry-1.docker.io/$hub_ns"
        echo "chart flint-lite-operator $op_version released."
    fi

    # The flint-lean chart. It was NOT gated here until 2026-08-26, and
    # the omission cost exactly what this script exists to prevent: the
    # chart execs /usr/local/bin/flint-lean-operator out of the
    # flint-lite-operator image, and that binary was not in it — an
    # install was a CrashLoopBackOff on "no such file or directory".
    # The 1.2.0 unpublished-image bug, one layer deeper: the image
    # existed and the binary inside it did not.
    #
    # Two images must exist: the operator image the chart names, and the
    # SIDECAR image, which is the one a workspace pod actually runs —
    # the webhook injects it, so an unpublished sidecar is a fleet of
    # pods that never start, with the operator itself perfectly healthy.
    lean_dir="$repo_root/flint-lean-chart"
    if [ -d "$lean_dir" ]; then
        lean_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$lean_dir/Chart.yaml'))['version'])")
        lean_app=$(python3 -c "import yaml; print(yaml.safe_load(open('$lean_dir/Chart.yaml'))['appVersion'])")
        lean_op_img=$(python3 -c "import yaml; print(yaml.safe_load(open('$lean_dir/values.yaml'))['image']['name'])")
        lean_sc_img=$(python3 -c "import yaml; print(yaml.safe_load(open('$lean_dir/values.yaml'))['sidecarImage']['name'])")
        for img in "$lean_op_img" "$lean_sc_img"; do
            if ! tag_exists "$img" "$lean_app"; then
                echo "REFUSING to push flint-lean $lean_version:" \
                     "$hub_ns/$img:$lean_app is not on Docker Hub." >&2
                exit 1
            fi
        done
        # The binaries the chart EXECS must be in the image it pulls.
        # tag_exists proves the image was published; it cannot prove the
        # image carries flint-lean-operator, so check the recipe that
        # builds it. Cheap, and it is the exact miss above.
        op_recipe="$repo_root/spdk-csi-driver/docker/Dockerfile.operator.prebuilt"
        for bin in flint-lean-operator flint-lean-gateway; do
            if ! grep -q "/usr/local/bin/$bin" "$op_recipe"; then
                echo "REFUSING to push flint-lean $lean_version: the chart execs" \
                     "/usr/local/bin/$bin but $(basename "$op_recipe") does not" \
                     "install it — the image would start and the binary would not exist." >&2
                exit 1
            fi
        done
        # Same question for the sidecar, plus the two things a lean
        # sidecar base MUST have: a shell (the injected startupProbe
        # execs `test -f` inside it) and ca-certificates (the S3 client
        # resolves rustls to rustls-native-certs — the SYSTEM trust
        # store — so a certless base fails every HTTPS endpoint, which
        # no kind rig can catch because MinIO is plain HTTP).
        sync_recipe="$repo_root/spdk-csi-driver/docker/Dockerfile.sync.prebuilt"
        if [ ! -f "$sync_recipe" ]; then
            echo "REFUSING to push flint-lean $lean_version: no build recipe for the" \
                 "sidecar image ($sync_recipe)." >&2
            exit 1
        fi
        if ! grep -q 'ca-certificates' "$sync_recipe"; then
            echo "REFUSING to push flint-lean $lean_version: $(basename "$sync_recipe")" \
                 "installs no ca-certificates; the sidecar reads the SYSTEM trust store" \
                 "and would fail every HTTPS S3 endpoint." >&2
            exit 1
        fi
        # The checked-in CRD is install-time bootstrap for the operator's
        # compiled-in copy — same rule as flintshares.yaml above.
        lean_gen=$(cd "$repo_root/spdk-csi-driver" && cargo run --quiet --bin crdgen -- lean 2>/dev/null || true)
        if [ -n "$lean_gen" ] && [ -f "$lean_dir/crds/flintleanworkspaces.yaml" ]; then
            if ! printf '%s\n' "$lean_gen" | diff -q - "$lean_dir/crds/flintleanworkspaces.yaml" >/dev/null; then
                echo "REFUSING to push flint-lean $lean_version:" \
                     "crds/flintleanworkspaces.yaml is stale." >&2
                exit 1
            fi
        fi
        helm package "$lean_dir" --destination "$pkg_dir" >/dev/null
        lean_pkg="$pkg_dir/flint-lean-$lean_version.tgz"
        echo "── pushing $(basename "$lean_pkg") to oci://registry-1.docker.io/$hub_ns"
        helm push "$lean_pkg" "oci://registry-1.docker.io/$hub_ns"
        echo "chart flint-lean $lean_version released."
    fi
fi
