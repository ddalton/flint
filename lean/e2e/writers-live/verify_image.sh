#!/bin/sh
# verify_image.sh — prove, ON A NODE, that a containerd image carries the
# expected bytes at a path. Meant to run over SSM (as root: ctr needs the
# containerd socket). POSIX sh; needs ctr, sha256sum, awk, mktemp.
#
#   verify_image.sh [options] <image-ref> <path-in-image> <sha256> [<path> <sha256> ...]
#
#   -n, --namespace NS   containerd namespace (default k8s.io: the kubelet's CRI
#                        only sees images in k8s.io; ctr's own default is
#                        "default", where the kubelet sees NOTHING)
#   --platform P         passed to `ctr images mount` (default: the node's own)
#   --snapshotter S      passed to `ctr images mount` (default: ctr's, overlayfs)
#   --digest sha256:X    the image's target digest in containerd must be X
#                        (oci_derive.py's image.manifest_digest, OCI format)
#   --no-cri             skip the crictl check
#
# WHAT IT PROVES. It mounts the image's UNPACKED SNAPSHOT read-only
# (`ctr images mount`: a View of the chain id of the config's diff_ids — the
# rootfs a container would start from), sha256sums each path, unmounts with
# --rm (which also drops the mount's lease) and compares. A path reached
# through a symlink is refused rather than followed: an absolute link would
# resolve against the HOST's root, not the image's. If crictl is present it
# also asks the CRI (the kubelet's view) to inspect the ref.
#
# Exit codes (checked in this order):
#   0 every path matches
#   2 usage
#   5 ctr missing, `ctr images ls` failed, or the mount failed
#   3 the image is not in the namespace
#   7 --digest given and the image's target digest differs
#   6 crictl present and the CRI does not see the image
#   4 a path is absent, not a regular file, or reached through a symlink
#   1 a path's sha256 differs (4 wins if both happen)
#
# IMPORT (from oci_derive.py's archive):
#
#   ctr -n k8s.io images import --platform linux/amd64 <tar>
#
#  · -n k8s.io is load-bearing; without it the kubelet never sees the image.
#  · --platform linux/amd64: the archive holds one amd64 image, so on an
#    x86_64 node this equals the default; it is explicit so an arm64 node
#    fails loudly instead of importing content it cannot unpack.
#  · --all-platforms: NOT needed and no effect on kubelet visibility for a
#    single-platform archive (in containerd 2.x it still only unpacks the
#    default platform). On a partial multi-arch archive it demands content
#    for every platform and fails.
#  · --no-unpack: do NOT use. The image is registered but has no snapshot;
#    the CRI unpacks it at container create (internal/cri/opts WithNewSnapshot),
#    so a bad layer (diff_id mismatch) surfaces at pod start instead of at
#    import. Note this script cannot tell: `ctr images mount` unpacks too.
#  · Don't pass --base-name (it FILTERS names to that prefix). Without it the
#    name comes from the archive verbatim: OCI index annotation
#    io.containerd.image.name, or docker manifest.json RepoTags (normalized).
#  · The ref: containerd stores and ctr looks up names VERBATIM; the kubelet's
#    CRI normalizes `dilipdalton/x:tag` to `docker.io/dilipdalton/x:tag` and
#    looks THAT up. So the containerd name must carry docker.io/ (oci_derive.py
#    normalizes it), while the pod spec / chart value may use either form.
#    This script normalizes whatever it is given the same way.
#  · The pull policy must not be Always for a tag that exists only on nodes
#    (the lean worker pod is IfNotPresent; the charts default IfNotPresent);
#    a node that joins after the import has no image — import on every node.
#    The kubelet's image GC may delete an imported image no pod uses yet
#    when the disk crosses imageGCHighThresholdPercent.

set -u

usage() {
    sed -n '2,/^# IMPORT/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

# docker reference normalization, as containerd's reference.ParseDockerRef
normalize_ref() {
    _ref=$1
    case "$_ref" in
        *@*) _dig="@${_ref#*@}"; _name=${_ref%%@*} ;;
        *) _dig=""; _name=$_ref ;;
    esac
    _domain=""
    _path=$_name
    case "$_name" in
        */*)
            _first=${_name%%/*}
            case "$_first" in
                *.*|*:*|localhost) _domain=$_first; _path=${_name#*/} ;;
            esac
            ;;
    esac
    [ -z "$_domain" ] && _domain=docker.io
    [ "$_domain" = index.docker.io ] && _domain=docker.io
    if [ "$_domain" = docker.io ]; then
        case "$_path" in */*) ;; *) _path="library/$_path" ;; esac
    fi
    case "${_path##*/}" in
        *:*) ;;
        *) [ -z "$_dig" ] && _path="$_path:latest" ;;
    esac
    printf '%s/%s%s\n' "$_domain" "$_path" "$_dig"
}

NS=k8s.io
PLATFORM=""
SNAP=""
WANT_DIGEST=""
CRI=1
while [ $# -gt 0 ]; do
    case "$1" in
        -n|--namespace) [ $# -ge 2 ] || { usage >&2; exit 2; }; NS=$2; shift 2 ;;
        --platform) [ $# -ge 2 ] || { usage >&2; exit 2; }; PLATFORM=$2; shift 2 ;;
        --snapshotter) [ $# -ge 2 ] || { usage >&2; exit 2; }; SNAP=$2; shift 2 ;;
        --digest) [ $# -ge 2 ] || { usage >&2; exit 2; }; WANT_DIGEST=$2; shift 2 ;;
        --no-cri) CRI=0; shift ;;
        -h|--help) usage; exit 0 ;;
        --) shift; break ;;
        -*) echo "verify_image: unknown option $1" >&2; usage >&2; exit 2 ;;
        *) break ;;
    esac
done
if [ $# -lt 3 ] || [ $(( ($# - 1) % 2 )) -ne 0 ]; then
    usage >&2
    exit 2
fi
RAW_REF=$1
shift
REF=$(normalize_ref "$RAW_REF")

# validate every pair before touching containerd
N=0
for _a in "$@"; do
    N=$((N + 1))
    if [ $((N % 2)) -eq 1 ]; then
        case "$_a" in
            /*) ;;
            *) echo "verify_image: path $_a must be absolute" >&2; exit 2 ;;
        esac
        case "$_a/" in
            */../*|*/./*|*//*) echo "verify_image: path $_a must be normalized" >&2; exit 2 ;;
        esac
    else
        _h=$(printf '%s' "${_a#sha256:}" | tr 'A-F' 'a-f')
        if [ ${#_h} -ne 64 ] || [ -n "$(printf '%s' "$_h" | tr -d '0-9a-f')" ]; then
            echo "verify_image: $_a is not a sha256" >&2
            exit 2
        fi
    fi
done

for _tool in ctr sha256sum awk mktemp; do
    command -v "$_tool" >/dev/null 2>&1 || { echo "verify_image: $_tool not found" >&2; exit 5; }
done

echo "image $REF (namespace $NS)"
[ "$REF" != "$RAW_REF" ] && echo "  normalized from $RAW_REF"

if ! LIST=$(ctr -n "$NS" images ls -q 2>&1); then
    echo "FAILED: ctr -n $NS images ls: $LIST"
    exit 5
fi
if ! printf '%s\n' "$LIST" | grep -Fxq -- "$REF"; then
    echo "ABSENT: $REF is not an image in namespace $NS"
    _repo=${REF%@*}
    _repo=${_repo%:*}
    _similar=$(printf '%s\n' "$LIST" | grep -F -- "$_repo" | head -5)
    [ -n "$_similar" ] && printf '  same repository:\n%s\n' "$(printf '%s\n' "$_similar" | sed 's/^/    /')"
    exit 3
fi

if [ -n "$WANT_DIGEST" ]; then
    GOT_DIGEST=$(ctr -n "$NS" images ls 2>/dev/null | awk -v r="$REF" '$1 == r { print $3 }')
    if [ "$GOT_DIGEST" != "$WANT_DIGEST" ]; then
        echo "DIGEST MISMATCH: $REF targets ${GOT_DIGEST:-<unknown>}, expected $WANT_DIGEST"
        exit 7
    fi
    echo "  digest $GOT_DIGEST"
fi

if [ "$CRI" = 1 ] && [ "$NS" = k8s.io ]; then
    if command -v crictl >/dev/null 2>&1; then
        _ep=""
        if [ -z "${CONTAINER_RUNTIME_ENDPOINT:-}" ] && [ ! -f /etc/crictl.yaml ] \
                && [ -S /run/containerd/containerd.sock ]; then
            _ep="--runtime-endpoint unix:///run/containerd/containerd.sock"
        fi
        # shellcheck disable=SC2086
        if crictl $_ep inspecti "$REF" >/dev/null 2>&1; then
            echo "  CRI sees it (crictl inspecti)"
        else
            echo "CRI DOES NOT SEE $REF (crictl inspecti failed): the kubelet cannot use it"
            exit 6
        fi
    else
        echo "  note: crictl not found; the kubelet's view was not checked"
    fi
fi

MNT=$(mktemp -d "${TMPDIR:-/tmp}/verify-image.XXXXXX") || { echo "FAILED: mktemp"; exit 5; }
cleanup() {
    # always: unmount (a no-op if never mounted), drop the snapshot + lease, remove the dir
    ctr -n "$NS" images unmount --rm "$MNT" >/dev/null 2>&1 || echo "warning: ctr images unmount --rm $MNT failed" >&2
    rmdir "$MNT" 2>/dev/null || echo "warning: $MNT not removed" >&2
}
trap cleanup EXIT
trap 'exit 130' INT TERM

MOPTS=""
[ -n "$PLATFORM" ] && MOPTS="$MOPTS --platform $PLATFORM"
[ -n "$SNAP" ] && MOPTS="$MOPTS --snapshotter $SNAP"
# shellcheck disable=SC2086
if ! MOUT=$(ctr -n "$NS" images mount $MOPTS "$REF" "$MNT" 2>&1); then
    echo "MOUNT FAILED: ctr images mount $REF: $MOUT"
    exit 5
fi

RC=0
ABSENT=0
MISMATCH=0
set -f
while [ $# -ge 2 ]; do
    P=$1
    WANT=$(printf '%s' "${2#sha256:}" | tr 'A-F' 'a-f')
    shift 2
    BAD=""
    CUR=""
    _oldifs=$IFS
    IFS=/
    for C in $P; do
        [ -z "$C" ] && continue
        CUR="$CUR/$C"
        if [ -L "$MNT$CUR" ]; then
            BAD="$CUR is a symlink -> $(readlink "$MNT$CUR" 2>/dev/null)"
            break
        fi
    done
    IFS=$_oldifs
    if [ -n "$BAD" ]; then
        echo "UNVERIFIABLE $P: $BAD (pass the resolved path)"
        ABSENT=1
        continue
    fi
    if [ ! -f "$MNT$P" ]; then
        echo "ABSENT $P: not a regular file in the image"
        ABSENT=1
        continue
    fi
    GOT=$(sha256sum "$MNT$P" | awk '{ print $1 }')
    META=$(ls -ln "$MNT$P" | awk '{ print $1, $3 ":" $4, $5 " bytes" }')
    if [ "$GOT" = "$WANT" ]; then
        echo "OK $P sha256=$GOT ($META)"
    else
        echo "MISMATCH $P expected=$WANT actual=$GOT ($META)"
        MISMATCH=1
    fi
done
set +f

if [ "$ABSENT" = 1 ]; then
    RC=4
elif [ "$MISMATCH" = 1 ]; then
    RC=1
fi
if [ "$RC" = 0 ]; then
    echo "VERIFIED $REF"
else
    echo "FAILED $REF (exit $RC)"
fi
exit "$RC"
