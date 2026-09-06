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

# Binaries from the HUB crate (spdk-csi-driver).
#
# `flint-lean-operator` was missing here until 2026-08-26 while
# Dockerfile.operator.prebuilt COPYs it — so the lean half of the
# operator image was staged BY HAND, outside every staleness check this
# script exists to enforce. That is the same shape as the bug the
# 1.38.0 changelog records ("the chart execs /usr/local/bin/
# flint-lean-operator and that binary was not in it"), and the same
# shape as the 1.30.0 near-miss described above.
# flint-lean-gateway is NOT optional in any scope, and that is a
# property of the recipe rather than a preference: Dockerfile.operator.
# prebuilt COPYs it unconditionally, so a scope that omits it does not
# produce an operator image without it — it produces no image at all,
# failing the release at `docker build` with "COPY failed".
#
# flint-s3-csi-node and flint-s3-broker are the s3.csi.chert.us image
# (Dockerfile.s3csi.prebuilt); the worker that image's plugin launches
# comes from its own crate, below.
BINS="csi-driver flint-nfs-server flint-pnfs-mds flint-pnfs-ds flint-lite-operator flint-hub-gateway flint-lean-operator flint-s3-csi-node flint-s3-broker"

# A LEAN-SCOPED release (the 1.38.0 shape: only the flint-lean chart and
# the two images it pulls) republishes the operator image and the sidecar
# image, and nothing else. Demanding csi-driver and the pNFS binaries be
# fresh for that is a refusal with no safety content — and it is almost
# certainly why the lean binaries were hand-staged in the first place,
# which is the hole this script just grew to cover. `lean` stages exactly
# what those two images COPY, with the SAME staleness rules.
#
# An S3CSI-scoped release publishes the flint-s3-csi chart (the node
# DaemonSet + broker image and the two worker images), the CRD-only
# flint-passthrough chart, and the mounter base image. The mounter base
# carries no flint binary — it is a Debian base with mount-s3 from AWS's
# .deb — so it stages nothing here. `passthrough` is accepted as the
# same scope: that chart is now the CRD, and its delivery is this one.
SCOPE=${1:-all}
case "$SCOPE" in
    all)  ;;
    lean) BINS="flint-lite-operator flint-hub-gateway flint-lean-operator" ;;
    s3csi|passthrough)
          SCOPE=s3csi; BINS="flint-s3-csi-node flint-s3-broker" ;;
    # A FORGE-scoped release publishes three images of its own and
    # touches none of the others: the controller and the door
    # (Dockerfile.forge-operator.prebuilt), the syncer
    # (Dockerfile.forge-syncer.prebuilt, which also carries flint-sync
    # for the legible export), and the git server
    # (Dockerfile.forge-git, which carries the hook). Its binaries come
    # from THREE crates, which is why it has its own clock below.
    forge) BINS="flint-forge-operator flint-hub-gateway" ;;
    *)    echo "usage: stage-prebuilt.sh [all|lean|s3csi|forge]" >&2; exit 2 ;;
esac

# Binaries from the LEAN crate (lean/sidecar) — a separate crate with a
# separate target dir, which is why they could not simply join $BINS.
# `flint-sync` is the image every workspace pod actually RUNS, and it was
# published entirely by hand: absent from this script AND from
# publish-images.sh, with only release.sh's after-the-fact "is it on the
# Hub?" check standing between it and a silent wrong-code release.
LEAN_BINS="flint-sync flint-lean-gateway"

# An s3csi release publishes neither lean image, so it stages neither
# lean binary; it stages the worker crate's binary instead (below).
if [ "$SCOPE" = s3csi ]; then
    LEAN_BINS=""
fi
# A forge release stages `flint-sync` and nothing else from the lean
# crate: the syncer image COPYs it, because the legible export runs the
# SHIPPED lean binary rather than reimplementing lean's publish
# ordering. Omitting it does not produce an export-less image — it
# produces no image at all, at "COPY failed".
if [ "$SCOPE" = forge ]; then
    LEAN_BINS="flint-sync"
fi

# Binaries from the FORGE crate (forge/syncer). Only a forge-scoped
# release stages them.
FORGE_BINS=""
if [ "$SCOPE" = forge ] || [ "$SCOPE" = all ]; then
    FORGE_BINS="flint-forge-syncer"
fi

# Binaries from the WORKER crate (crates/flint-s3-worker): PID 1 of
# every worker pod the s3.csi.chert.us plugin creates, and the payload both
# worker images COPY (Dockerfile.s3worker, Dockerfile.s3worker-lean).
WORKER_BINS="flint-s3-worker"
if [ "$SCOPE" = lean ] || [ "$SCOPE" = forge ]; then
    WORKER_BINS=""
fi

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

# The lean crate has its OWN newest-source clock, and it must include
# crates/flint-store: the sidecar links it, so a store edit changes the
# binary with no lean/sidecar/src file touched at all. That is exactly
# the Cargo.lock argument above, one crate further out.
lean_crate=$(cd "$here/../lean/sidecar" && pwd)
store_crate=$(cd "$here/../crates/flint-store" && pwd)
# There is no workspace Cargo.lock at the repo root — the lean crate
# carries its own. Naming a path that does not exist made `find` fail,
# and under `set -euo pipefail` that killed this script with NO message
# at all, which is the worst possible failure for a staleness gate.
newest_lean=$(find "$lean_crate/src" "$lean_crate/Cargo.toml" "$lean_crate/Cargo.lock" \
                   "$store_crate/src" "$store_crate/Cargo.toml" \
                   -type f -print0 \
              | xargs -0 stat -f '%m %N' | sort -rn | head -1)
lean_mtime=${newest_lean%% *}
lean_name=${newest_lean#* }
case "$lean_mtime" in
    ''|*[!0-9]*)
        echo "cannot determine the newest LEAN source mtime — refusing to stage blind" >&2
        exit 2 ;;
esac
echo "newest lean source: $(date -r "$lean_mtime" '+%Y-%m-%d %H:%M:%S')  ${lean_name#$here/../}"

# The worker crate (crates/flint-s3-worker) has its own clock too. It
# links nothing of ours, so its own sources and lockfile are the whole
# of it.
# The forge crate has the same shape as the lean one: it links
# crates/flint-store, so a store edit changes its binaries with no
# forge/syncer/src file touched at all.
forge_crate=$(cd "$here/../forge/syncer" && pwd)
if [ -n "$FORGE_BINS" ]; then
    newest_forge=$(find "$forge_crate/src" "$forge_crate/Cargo.toml" "$forge_crate/Cargo.lock" \
                        "$store_crate/src" "$store_crate/Cargo.toml" \
                        -type f -print0 \
                   | xargs -0 stat -f '%m %N' | sort -rn | head -1)
    forge_mtime=${newest_forge%% *}
    forge_name=${newest_forge#* }
    case "$forge_mtime" in
        ''|*[!0-9]*)
            echo "cannot determine the newest FORGE source mtime — refusing to stage blind" >&2
            exit 2 ;;
    esac
    echo "newest forge source: $(date -r "$forge_mtime" '+%Y-%m-%d %H:%M:%S')  ${forge_name#$here/../}"
fi

worker_crate=$(cd "$here/../crates/flint-s3-worker" && pwd)
newest_worker=$(find "$worker_crate/src" "$worker_crate/Cargo.toml" "$worker_crate/Cargo.lock" \
                     -type f -print0 \
                | xargs -0 stat -f '%m %N' | sort -rn | head -1)
worker_mtime=${newest_worker%% *}
worker_name=${newest_worker#* }
case "$worker_mtime" in
    ''|*[!0-9]*)
        echo "cannot determine the newest WORKER source mtime — refusing to stage blind" >&2
        exit 2 ;;
esac
echo "newest worker source: $(date -r "$worker_mtime" '+%Y-%m-%d %H:%M:%S')  ${worker_name#$here/../}"

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
    for b in $FORGE_BINS; do
        src="$forge_crate/target/$triple/release/$b"
        if [ ! -f "$src" ]; then
            echo "  ✗ MISSING $arch/$b — build it before staging" >&2
            echo "            (cd forge/syncer && cargo zigbuild --release --features s3 \\" >&2
            echo "               --target $triple)" >&2
            stale=1; continue
        fi
        m=$(stat -f '%m' "$src")
        if [ "$m" -lt "$forge_mtime" ]; then
            echo "  ✗ STALE   $arch/$b built $(date -r "$m" '+%m-%d %H:%M') — older than the forge source" >&2
            stale=1; continue
        fi
        cp "$src" "$dest/$arch/$b"
        echo "  ✓ staged  $arch/$b  ($(date -r "$m" '+%m-%d %H:%M'), $(( $(stat -f '%z' "$src") / 1048576 )) MiB)"
    done
    for b in $LEAN_BINS; do
        src="$lean_crate/target/$triple/release/$b"
        if [ ! -f "$src" ]; then
            echo "  ✗ MISSING $arch/$b — build it before staging" >&2
            echo "            (cd lean/sidecar && cargo zigbuild --release --features s3 \\" >&2
            echo "               --target $triple)" >&2
            stale=1; continue
        fi
        m=$(stat -f '%m' "$src")
        if [ "$m" -lt "$lean_mtime" ]; then
            echo "  ✗ STALE   $arch/$b built $(date -r "$m" '+%m-%d %H:%M') — older than the lean source" >&2
            stale=1; continue
        fi
        cp "$src" "$dest/$arch/$b"
        echo "  ✓ staged  $arch/$b  ($(date -r "$m" '+%m-%d %H:%M'), $(( $(stat -f '%z' "$src") / 1048576 )) MiB)"
    done
    for b in $WORKER_BINS; do
        src="$worker_crate/target/$triple/release/$b"
        if [ ! -f "$src" ]; then
            echo "  ✗ MISSING $arch/$b — build it before staging" >&2
            echo "            (cd crates/flint-s3-worker && cargo zigbuild --release --target $triple)" >&2
            stale=1; continue
        fi
        m=$(stat -f '%m' "$src")
        if [ "$m" -lt "$worker_mtime" ]; then
            echo "  ✗ STALE   $arch/$b built $(date -r "$m" '+%m-%d %H:%M') — older than the worker source" >&2
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
# Still the LITE chart's appVersion under `lean` scope, and deliberately:
# this checks the hub image the operator binary has pinned at compile
# time, which does not move in a lean-scoped release. If it disagrees,
# the binary was built from a different tree — the exact failure an
# mtime cannot see.
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
