#!/bin/bash
# ---------------------------------------------------------------------------
# Refresh (or check) the Chainguard base-image digest pins in the flint
# Dockerfiles.
#
#   scripts/refresh-chainguard-bases.sh           # rewrite stale pins in place
#   scripts/refresh-chainguard-bases.sh --check   # exit 1 if any pin is behind
#
# WHY THIS EXISTS
#
# Every runtime image is `ARG BASE_IMAGE=cgr.dev/chainguard/<name>:<tag>@sha256:...`
# (docs/chainguard-migration.md). The digest makes a build reproducible,
# and it also FREEZES the base: Chainguard rebuilds `:latest` as CVEs are
# fixed, and a pin nobody moves ships last month's CVEs forever — the
# opposite of why the images moved to Chainguard. So a release refreshes
# the pins (and rebuilds), and `--check` says whether that was done.
#
# WHAT IT DOES
#
# For every `ARG BASE_IMAGE=cgr.dev/chainguard/...` line under
# spdk-csi-driver/docker/, resolve the tag's CURRENT multi-arch INDEX
# digest with `docker buildx imagetools inspect` (a registry read; nothing
# is pulled), require that index to carry linux/amd64 AND linux/arm64
# (publish-images.sh builds both), and compare it with the pin. It
# rewrites only the digest; the name and tag are the Dockerfile's choice.
# It does not rebuild, push, or touch release.sh.
#
# EXIT STATUS
#
#   0  every pin is current (--check), or every stale pin was rewritten
#   1  --check only: at least one pin is behind, or a base is not pinned
#   2  a digest could not be resolved or failed validation. Never read
#      as "current": a registry error is not a verdict.
#
# bash 3.2-safe (macOS /bin/bash): no associative arrays.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
docker_dir=$(cd "$here/../spdk-csi-driver/docker" && pwd)
pin_re='^ARG BASE_IMAGE=cgr\.dev/chainguard/'

mode=write
case "${1:-}" in
    "") ;;
    --check) mode=check ;;
    -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
    *) echo "usage: $(basename "$0") [--check]" >&2; exit 2 ;;
esac

# Validate the registry's answer and print the index digest, or fail.
validate_py='
import json, re, sys
ref, raw = sys.argv[1], sys.argv[2]
try:
    m = json.loads(raw)
except ValueError:
    sys.exit(f"{ref}: registry answer is not JSON: {raw[:200]}")
d = m.get("digest", "")
if not re.fullmatch(r"sha256:[0-9a-f]{64}", d):
    sys.exit(f"{ref}: no index digest in the registry answer")
plats = set()
for p in m.get("manifests") or []:
    pl = p.get("platform") or {}
    plats.add(pl.get("os", "") + "/" + pl.get("architecture", ""))
missing = {"linux/amd64", "linux/arm64"} - plats
if missing:
    sys.exit(f"{ref}: index {d} lacks {sorted(missing)}; not a usable multi-arch base")
print(d)
'

# Replace one exact line in a file (argv: path old new).
rewrite_py='
import sys
path, old, new = sys.argv[1:4]
text = open(path).read()
lines = text.split("\n")
if old not in lines:
    sys.exit(f"{path}: pin line not found at rewrite time")
open(path, "w").write("\n".join(new if l == old else l for l in lines))
'

cache=""   # lines of "<image:tag> <digest>"
resolve() {  # <image:tag> -> prints digest, or returns 1
    local ref=$1 hit json
    hit=$(printf '%s\n' "$cache" | awk -v r="$ref" '$1 == r { print $2; exit }')
    if [ -n "$hit" ]; then printf '%s\n' "$hit"; return 0; fi
    if ! json=$(docker buildx imagetools inspect "$ref" --format '{{json .Manifest}}' 2>&1); then
        echo "cannot resolve $ref: $json" >&2
        return 1
    fi
    python3 -c "$validate_py" "$ref" "$json"
}

files=$(grep -lE "$pin_re" "$docker_dir"/Dockerfile* 2>/dev/null | sort || true)
if [ -z "$files" ]; then
    echo "no 'ARG BASE_IMAGE=cgr.dev/chainguard/...' lines under $docker_dir" >&2
    exit 2
fi

behind=0
for f in $files; do
    rel=${f#"$docker_dir/"}
    # Read the pin lines BEFORE any rewrite of this file.
    pins=$(grep -E "$pin_re" "$f")
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        val=${line#ARG BASE_IMAGE=}
        ref=${val%%@*}
        pinned=""
        [ "$ref" != "$val" ] && pinned=${val#*@}
        want=$(resolve "$ref") || exit 2
        cache=$(printf '%s\n%s %s' "$cache" "$ref" "$want")
        if [ "$pinned" = "$want" ]; then
            echo "current  $rel  $ref@$want"
            continue
        fi
        behind=1
        if [ -z "$pinned" ]; then
            echo "UNPINNED $rel  $ref  (current $want)"
        else
            echo "BEHIND   $rel  $ref@$pinned -> $want"
        fi
        if [ "$mode" = write ]; then
            python3 -c "$rewrite_py" "$f" "$line" "ARG BASE_IMAGE=$ref@$want" || exit 2
            echo "         rewrote $rel"
        fi
    done <<EOF
$pins
EOF
done

if [ "$mode" = check ] && [ "$behind" = 1 ]; then
    echo "Chainguard base pins are behind or unpinned: run scripts/refresh-chainguard-bases.sh, rebuild, retest." >&2
    exit 1
fi
exit 0
