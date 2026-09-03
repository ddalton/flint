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
    echo "usage: $0 [check|images|chart|all] [all|lean|s3csi] [--force-republish]" >&2; exit 2 ;;
esac

# SCOPE, matching stage-prebuilt.sh and publish-images.sh. A lean-scoped
# release (the 1.38.0/1.39.0 shape) publishes the flint-lean chart and
# the two images it pulls; the CSI, lite and lite-operator charts are
# untouched and must not be republished. A passthrough-scoped release
# (the 1.40.0 shape) is the same idea for the third front end: the
# flint-passthrough chart, the shared operator image, and the mounter.
#
# Adding the scope to the image
# scripts and NOT here is how the 1.39.0 release re-pushed three
# unrelated charts at their EXISTING versions — see push_chart below for
# why that is not harmless.
scope=all
force_republish=0
# Shift the command off first, then validate what remains STRICTLY. An
# unrecognized word must not be silently ignored: `chart len` would
# otherwise run in `all` scope and republish three charts the caller was
# explicitly trying to leave alone — the exact outcome this flag exists
# to prevent, reached by a typo.
shift || true
for a in "$@"; do
    case "$a" in
        lean) scope=lean ;;
        s3csi|passthrough) scope=s3csi ;;
        all)  scope=all ;;
        --force-republish) force_republish=1 ;;
        *) echo "unknown argument '$a' — usage: $0 [check|images|chart|all] [all|lean|s3csi] [--force-republish]" >&2
           exit 2 ;;
    esac
done

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
    # The PREBUILT file, because that is what publish-images.sh builds
    # and pushes. Naming the other one meant the gate cited a Dockerfile
    # the publisher never touches — which is how b52a423's setcap could
    # be present in the gated file and absent from the shipped image.
    print(pnfs["image"]["name"], pnfs["image"]["tag"],
          "spdk-csi-driver", "docker/Dockerfile.pnfs.prebuilt")
PYEOF
)

tag_exists() {  # <name> <tag> -> 0 if published on Docker Hub
    curl -fsS -o /dev/null \
        "https://hub.docker.com/v2/repositories/$hub_ns/$1/tags/$2" 2>/dev/null
}

# Push one chart, or say precisely why not.
#
# TWO refusals, and the second is the one that bit 1.39.0:
#
#  - OUT OF SCOPE. A lean release has no business touching the CSI or
#    lite charts.
#  - ALREADY PUBLISHED at this version. `helm package` is not
#    byte-deterministic, so re-pushing an unchanged chart at a version
#    that already exists MUTATES that version's digest on the registry.
#    Nothing breaks for `helm install --version`, but a released
#    artifact silently becoming different bytes is the kind of thing a
#    supply chain is supposed to make impossible, and it happens by
#    accident because `chart` pushed all four charts unconditionally.
#    Bump the version, or pass --force-republish and mean it.
push_chart() {  # <scopes> <name> <version> <tgz>
    local scopes=$1 name=$2 version=$3 pkg=$4
    case " $scopes " in
        *" $scope "*) ;;
        *) echo "  · skipping chart $name $version (scope=$scope)"; return 0 ;;
    esac
    if [ "$force_republish" = 0 ] && tag_exists "$name" "$version"; then
        echo "  · skipping chart $name $version — ALREADY on Docker Hub." \
             "Re-pushing would change its digest for identical content;" \
             "bump the version, or pass --force-republish."
        return 0
    fi
    echo "── pushing $(basename "$pkg") to oci://registry-1.docker.io/$hub_ns"
    helm push "$pkg" "oci://registry-1.docker.io/$hub_ns"
    echo "chart $name $version released."
}

# True when the active scope is in the given list.
#
# The chart GATES need this as much as push_chart does, and for a
# reason that only showed up once a second scope existed: every gate
# below runs unconditionally and EXITS on failure, so a chart that is
# out of scope can abort a release it has nothing to do with. Live
# example — the flint-lean chart's appVersion is 1.41.0 and its images
# are a pending publish, so a passthrough-scoped `chart` run died on
# lean's gate and never reached the passthrough chart at all. Scoping
# the push in 1.39.0 fixed half of this; this is the other half.
in_scope() {  # <space-separated scopes>
    case " $1 " in *" $scope "*) return 0 ;; *) return 1 ;; esac
}

tag_digest() {  # <name> <tag> -> the manifest-list digest, or empty
    curl -fsS "https://hub.docker.com/v2/repositories/$hub_ns/$1/tags/$2" 2>/dev/null \
        | python3 -c 'import json,sys; print(json.load(sys.stdin).get("digest") or "")' 2>/dev/null
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
    pkg_dir=$(mktemp -d)
    trap 'rm -rf "$pkg_dir"' EXIT

    if in_scope "all"; then
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
    helm package "$chart_dir" --destination "$pkg_dir" >/dev/null
    pkg="$pkg_dir/flint-csi-driver-chart-$chart_version.tgz"
    push_chart "all" flint-csi-driver-chart "$chart_version" "$pkg"
    fi

    # The flint-lite chart ships alongside as its OWN OCI artifact
    # (independent version; its appVersion pins the flint-pnfs tag, which
    # the gate above already verified via the pnfs image row).
    if in_scope "all"; then
    lite_dir="$repo_root/flint-lite-chart"
    lite_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$repo_root/flint-lite-chart/Chart.yaml'))['version'])")
    lite_app=$(python3 -c "import yaml; print(yaml.safe_load(open('$repo_root/flint-lite-chart/Chart.yaml'))['appVersion'])")
    if ! tag_exists flint-pnfs "$lite_app"; then
        echo "REFUSING to push flint-lite $lite_version:"              "$hub_ns/flint-pnfs:$lite_app (its appVersion default) is not on Docker Hub." >&2
        exit 1
    fi
    helm package "$lite_dir" --destination "$pkg_dir" >/dev/null
    lite_pkg="$pkg_dir/flint-lite-$lite_version.tgz"
    push_chart "all" flint-lite "$lite_version" "$lite_pkg"
    fi

    # The flint-lite OPERATOR chart, likewise its own artifact. Two
    # images must exist for it to be installable: its own (the
    # operator) and the hub image its appVersion makes the fleet
    # default — a fleet whose default hub image is unpublished is a
    # fleet of ImagePullBackOff, which is the 1.2.0 bug this script
    # exists to prevent, one level up.
    op_dir="$repo_root/flint-lite-operator-chart"
    if [ -d "$op_dir" ] && in_scope "all"; then
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
        push_chart "all" flint-lite-operator "$op_version" "$op_pkg"
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
    if [ -d "$lean_dir" ] && in_scope "all lean"; then
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
        # ...and the image the chart names must BE that image. The
        # recipe check above proves flint-lite-operator carries the lean
        # binaries; it says nothing about the lean-named alias, which is
        # a separate Docker Hub repo. Publish the alias with a build
        # instead of `imagetools create` (or forget to re-alias after a
        # rebuild) and the two names drift: the recipe check passes
        # against the lite image while the chart pulls stale bits from
        # the lean one. Digest equality is the only thing that closes
        # it, and it is one API call.
        if [ "$lean_op_img" != flint-lite-operator ]; then
            alias_d=$(tag_digest "$lean_op_img" "$lean_app")
            src_d=$(tag_digest flint-lite-operator "$lean_app")
            if [ -z "$alias_d" ] || [ -z "$src_d" ]; then
                echo "REFUSING to push flint-lean $lean_version: cannot read the" \
                     "manifest digest of $hub_ns/$lean_op_img:$lean_app or of" \
                     "$hub_ns/flint-lite-operator:$lean_app." >&2
                exit 1
            fi
            if [ "$alias_d" != "$src_d" ]; then
                echo "REFUSING to push flint-lean $lean_version:" \
                     "$hub_ns/$lean_op_img:$lean_app ($alias_d) is NOT the same" \
                     "image as $hub_ns/flint-lite-operator:$lean_app ($src_d)." \
                     "The lean image is an alias, not a rebuild — republish it with" \
                     "scripts/publish-images.sh, which uses 'buildx imagetools create'." >&2
                exit 1
            fi
            echo "  ✓ $hub_ns/$lean_op_img:$lean_app is $hub_ns/flint-lite-operator:$lean_app ($src_d)"
        fi
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
        push_chart "all lean" flint-lean "$lean_version" "$lean_pkg"
    fi

    # The flint-passthrough chart is the CRD alone: the s3.csi.chert.us node
    # driver (next) delivers the mount, and nothing in this chart runs.
    # The one gate a hand-written CRD needs: the shipped schema agrees
    # with the struct in both directions — a pruned field is a knob that
    # does nothing, and an extra property denies every pod that names
    # the CR.
    pt_dir="$repo_root/flint-passthrough-chart"
    if [ -d "$pt_dir" ] && in_scope "all s3csi"; then
        pt_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$pt_dir/Chart.yaml'))['version'])")
        ( cd "$repo_root/spdk-csi-driver" \
          && cargo test --quiet --lib passthrough::spec:: >/dev/null 2>&1 ) || {
            echo "REFUSING to push flint-passthrough $pt_version: the CRD/spec parity tests" \
                 "fail. Run: cargo test --lib passthrough::spec::" >&2
            exit 1; }
        helm package "$pt_dir" --destination "$pkg_dir" >/dev/null
        pt_pkg="$pkg_dir/flint-passthrough-$pt_version.tgz"
        push_chart "all s3csi" flint-passthrough "$pt_version" "$pt_pkg"
    fi

    # The flint-s3-csi chart: the node DaemonSet + broker image and the
    # two worker images, all at appVersion. Each is a pod that fails in
    # a way that names nothing if it is missing — the plugin never
    # registers, the worker never starts, the mount never appears.
    s3_dir="$repo_root/flint-s3-csi-chart"
    if [ -d "$s3_dir" ] && in_scope "all s3csi"; then
        s3_version=$(python3 -c "import yaml; print(yaml.safe_load(open('$s3_dir/Chart.yaml'))['version'])")
        s3_app=$(python3 -c "import yaml; print(yaml.safe_load(open('$s3_dir/Chart.yaml'))['appVersion'])")
        for img in flint-s3-csi flint-s3-worker flint-s3-worker-lean flint-passthrough-mounter; do
            if ! tag_exists "$img" "$s3_app"; then
                echo "REFUSING to push flint-s3-csi $s3_version:" \
                     "$hub_ns/$img:$s3_app is not on Docker Hub." >&2
                exit 1
            fi
        done
        # The worker image is FROM the mounter base, and the base is
        # where mount-s3 is PINNED: a moving mounter inside the data
        # plane is a change nobody's release notes chose.
        pt_recipe="$repo_root/spdk-csi-driver/docker/Dockerfile.passthrough"
        if ! grep -q 'mount-s3' "$pt_recipe" || ! grep -qE '^ARG MOUNT_S3_VERSION=[0-9]' "$pt_recipe"; then
            echo "REFUSING to push flint-s3-csi $s3_version: $(basename "$pt_recipe") does not" \
                 "install a PINNED mount-s3 — the worker image builds FROM it." >&2
            exit 1
        fi
        w_recipe="$repo_root/spdk-csi-driver/docker/Dockerfile.s3worker"
        if ! grep -q "flint-passthrough-mounter:$s3_app" "$w_recipe"; then
            echo "REFUSING to push flint-s3-csi $s3_version: $(basename "$w_recipe") is not FROM" \
                 "flint-passthrough-mounter:$s3_app (the release's mounter base)." >&2
            exit 1
        fi
        lw_recipe="$repo_root/spdk-csi-driver/docker/Dockerfile.s3worker-lean"
        if ! grep -q "flint-sync:$s3_app" "$lw_recipe"; then
            echo "REFUSING to push flint-s3-csi $s3_version: $(basename "$lw_recipe") is not FROM" \
                 "flint-sync:$s3_app (the release's sync image)." >&2
            exit 1
        fi
        ( cd "$repo_root/spdk-csi-driver" \
          && cargo test --quiet --lib s3csi:: >/dev/null 2>&1 ) || {
            echo "REFUSING to push flint-s3-csi $s3_version: the s3csi unit tests fail." >&2
            exit 1; }
        helm lint "$s3_dir" --set broker.static.secretRef=x >/dev/null || {
            echo "REFUSING to push flint-s3-csi $s3_version: helm lint fails." >&2; exit 1; }
        helm package "$s3_dir" --destination "$pkg_dir" >/dev/null
        s3_pkg="$pkg_dir/flint-s3-csi-$s3_version.tgz"
        push_chart "all s3csi" flint-s3-csi "$s3_version" "$s3_pkg"
    fi
fi
