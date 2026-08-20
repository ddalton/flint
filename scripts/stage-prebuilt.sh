#!/bin/bash
# ---------------------------------------------------------------------------
# Stage cross-compiled release binaries into docker/prebuilt/{amd64,arm64}/
# for the *.prebuilt Dockerfiles — and REFUSE to stage stale ones.
#
#   scripts/stage-prebuilt.sh
#
# WHY THIS EXISTS
#
# Staging used to be a bare `cp` from target/<triple>/release/. Nothing
# checked that those binaries came from the tree being released, and the
# 1.30.0 release nearly shipped 1.29.0-era code under new tags because
# the prebuilt dir still held the previous build. That failure is silent:
# the images build, push and run — they are just the wrong code, under a
# tag that says otherwise, and no test can catch it after the fact.
#
# The check: every binary must be NEWER than the newest source file that
# can change it. A stale binary is a hard refusal, never a warning,
# because a warning in a long release log is a thing you scroll past.
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
crate=$(cd "$here/../spdk-csi-driver" && pwd)
dest="$crate/docker/prebuilt"

BINS="csi-driver flint-nfs-server flint-pnfs-mds flint-pnfs-ds flint-lite-operator"

# Newest thing that can change a binary. Cargo.lock matters as much as
# src/ — a dependency bump with no source edit still changes the output.
newest_src=$(find "$crate/src" "$crate/Cargo.toml" "$crate/Cargo.lock" -type f -print0 \
             | xargs -0 stat -f '%m %N' | sort -rn | head -1)
src_mtime=${newest_src%% *}
src_name=${newest_src#* }
# An empty or non-numeric answer means the comparison below would silently
# pass everything — which is precisely the failure this script exists to
# prevent, so it is fatal rather than a fallback.
case "$src_mtime" in
    ''|*[!0-9]*)
        echo "cannot determine the newest source mtime — refusing to stage blind" >&2
        exit 2 ;;
esac
echo "newest source: $(date -r "$src_mtime" '+%Y-%m-%d %H:%M:%S')  ${src_name#$crate/}"

stale=0
for arch_pair in "x86_64:amd64" "aarch64:arm64"; do
    triple="${arch_pair%%:*}-unknown-linux-musl"
    arch="${arch_pair##*:}"
    mkdir -p "$dest/$arch"
    for b in $BINS; do
        src="$crate/target/$triple/release/$b"
        if [ ! -f "$src" ]; then
            echo "  ✗ MISSING $arch/$b — build it before staging" >&2
            stale=1; continue
        fi
        m=$(stat -f '%m' "$src")
        if [ "$m" -lt "$src_mtime" ]; then
            echo "  ✗ STALE   $arch/$b built $(date -r "$m" '+%m-%d %H:%M') — older than the source" >&2
            stale=1; continue
        fi
        cp "$src" "$dest/$arch/$b"
        echo "  ✓ staged  $arch/$b  ($(date -r "$m" '+%m-%d %H:%M'), $(( $(stat -f '%z' "$src") / 1048576 )) MiB)"
    done
done

# Content check, which an mtime cannot fake. The operator has the hub
# image pinned at COMPILE TIME, so the binary itself says which release
# it was built for — compare that against the chart's appVersion. This is
# what actually catches "rebuilt the wrong tree": a fresh timestamp on
# code from a stale checkout passes every mtime test there is.
want_pin=$(awk '/^appVersion:/ {gsub(/"/,"",$2); print $2}' \
           "$here/../flint-lite-chart/Chart.yaml")
if [ -n "$want_pin" ] && [ "$stale" = "0" ]; then
    for arch in amd64 arm64; do
        got=$(strings "$dest/$arch/flint-lite-operator" 2>/dev/null \
              | grep -oE 'dilipdalton/flint-pnfs:[0-9.]+' | sort -u | head -1)
        if [ "$got" != "dilipdalton/flint-pnfs:$want_pin" ]; then
            echo "  ✗ WRONG BUILD $arch/flint-lite-operator pins '${got:-<none>}'," >&2
            echo "                the chart's appVersion is $want_pin" >&2
            stale=1
        else
            echo "  ✓ pin check $arch: $got"
        fi
    done
fi

if [ "$stale" != "0" ]; then
    # Leave NOTHING behind. A half-staged directory is worse than an
    # empty one: the next run — or a careless `docker build` — would find
    # a plausible-looking prebuilt tree holding a mix of fresh and stale
    # binaries, which is the silent-wrong-code failure with extra steps.
    rm -rf "$dest"
    echo >&2
    echo "REFUSING to stage: at least one binary is stale or missing." >&2
    echo "Staging directory removed. Rebuild, then re-run — shipping these" >&2
    echo "would publish old code under a new tag." >&2
    exit 1
fi
echo "all binaries fresh; staged under $dest"
