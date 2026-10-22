#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# spdk-patches.sh -- Flint's SPDK patch series (spdk-csi-driver/spdk-patches/).
#
# The series is BASE (an upstream SPDK tag) plus numbered patches in
# `git format-patch` form; docker/Dockerfile.spdk applies them in order with
# `git apply`. Work on it in a scratch clone of upstream SPDK -- no fork:
#
#   scripts/spdk-patches.sh tree DIR [TAG]  clone SPDK at TAG (default: BASE)
#                                           into DIR; `git am -3` the series
#                                           onto branch flint, one commit each
#   scripts/spdk-patches.sh test DIR        build DIR with unit tests; run the
#                                           suites the series touches
#   scripts/spdk-patches.sh export DIR      rewrite the series (and BASE) from
#                                           DIR's commits on top of its tag
#   scripts/spdk-patches.sh check [TAG]     the series applies to TAG exactly
#                                           as the image build applies it
#
# Change a patch: tree; amend its commit (git rebase -i, or commit --fixup
# and rebase --autosquash); test; export. Add one: commit on top; test;
# export. Upgrade SPDK: `tree DIR v26.09`. `git am -3` merges a patch whose
# context moved and stops on a real conflict: resolve it, then
# `git -C DIR am --continue`. A patch upstream has merged applies empty:
# `git -C DIR am --skip`. Then test, export (which writes BASE), and the
# release gates. Review the diff of spdk-patches/ like any other change.
# ---------------------------------------------------------------------------
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
P=$here/spdk-csi-driver/spdk-patches
UPSTREAM=${SPDK_UPSTREAM:-https://github.com/spdk/spdk.git}
# the unit-test suites the series touches (blob, lvol, raid)
SUITES="test/unit/lib/blob/blob.c/blob_ut test/unit/lib/lvol/lvol.c/lvol_ut
test/unit/lib/bdev/vbdev_lvol.c/vbdev_lvol_ut test/unit/lib/bdev/raid/bdev_raid.c/bdev_raid_ut
test/unit/lib/bdev/raid/raid1.c/raid1_ut"

die() { echo "spdk-patches: $*" >&2; exit 1; }
git_id() {  # committer identity for am/rebase in the scratch clone
    git -c user.name="$(git -C "$here" config user.name || echo flint)" \
        -c user.email="$(git -C "$here" config user.email || echo flint@localhost)" "$@"
}

cmd_tree() {
    local dir=${1:?usage: tree DIR [TAG]} tag=${2:-$(cat "$P/BASE")}
    [ -e "$dir" ] && die "$dir already exists"
    git clone -q "$UPSTREAM" "$dir"
    git -C "$dir" checkout -q -b flint "$tag"
    git -C "$dir" config flint.base "$tag"
    git -C "$dir" submodule update -q --init --recursive
    if ! git_id -C "$dir" am -q -3 "$P"/[0-9]*.patch; then
        die "a patch stopped on $tag: resolve it in $dir and run 'git -C $dir am --continue' (or '--skip' when upstream already has it)"
    fi
    git -C "$dir" log --oneline "$tag"..flint
}

cmd_test() {
    local dir=${1:?usage: test DIR} stamp fails=0 ut log
    stamp=$(mktemp)
    log=$dir/spdk-patches-test.log
    # Needs SPDK's build dependencies (its scripts/pkgdep.sh; the image's
    # builder installs them), python3-tabulate among them. `make -k`:
    # spdk_top (needs ncurses headers) and nvmf's ctrlr_ut (does not link
    # with 0001) may fail; what counts is each suite below being rebuilt now
    # and passing.
    ( cd "$dir" && ./configure --enable-unit-tests --with-uring --with-ublk &&
      nice make -k -j"$(nproc)" ) > "$log" 2>&1 || true
    for ut in $SUITES; do
        if [ ! "$dir/$ut" -nt "$stamp" ]; then
            echo "FAIL $ut (not rebuilt)"; fails=$((fails + 1))
        elif (cd "$dir" && "./$ut" >/dev/null 2>&1); then
            echo "PASS $ut"
        else
            echo "FAIL $ut (run ./$ut in $dir)"; fails=$((fails + 1))
        fi
    done
    rm -f "$stamp"
    if [ "$fails" != 0 ]; then
        grep -E "error|Error [0-9]|No module named" "$log" | grep -v "Waiting for" | head -10 >&2 || true
        die "$fails suite(s) failed (build log: $log)"
    fi
    echo "SPDK-PATCHES TEST PASS"
}

cmd_export() {
    local dir=${1:?usage: export DIR} tag
    tag=$(git -C "$dir" config flint.base) || die "$dir was not made by '$0 tree'"
    [ -d "$dir/.git/rebase-apply" ] && die "$dir is in the middle of 'git am'"
    git -C "$dir" diff --quiet && git -C "$dir" diff --cached --quiet ||
        die "$dir has uncommitted changes"
    rm -f "$P"/[0-9]*.patch
    git -C "$dir" format-patch -q --zero-commit --no-signature -N -o "$P" "$tag"..HEAD
    echo "$tag" > "$P/BASE"
    ls "$P"
}

cmd_check() {
    local tag=${1:-$(cat "$P/BASE")} t f
    t=$(mktemp -d)
    # shellcheck disable=SC2064  # expand now: $t is local, the trap runs at exit
    trap "rm -rf '$t'" EXIT
    git clone -q --depth 1 --branch "$tag" "$UPSTREAM" "$t/spdk" 2>/dev/null || die "cannot clone $tag"
    for f in "$P"/[0-9]*.patch; do
        git -C "$t/spdk" apply "$f" || die "$(basename "$f") does not apply on $tag"
        echo "applies: $(basename "$f")"
    done
    echo "SPDK-PATCHES CHECK PASS on $tag"
}

case ${1:-} in
    tree | test | export | check) cmd=$1; shift; "cmd_$cmd" "$@" ;;
    *) sed -n '3,25p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
