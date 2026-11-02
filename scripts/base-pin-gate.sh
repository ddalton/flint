#!/bin/bash
# ---------------------------------------------------------------------------
# Base-pin gate: every Chainguard base image a Dockerfile pins must be its
# tag's CURRENT digest before an image is built or a chart is pushed.
#
#   scripts/base-pin-gate.sh            # refuse (exit 1) on a pin that is
#                                       # behind, unpinned or unresolvable
#   scripts/base-pin-gate.sh --report   # print the findings, never refuse
#
# Called by publish-images.sh (a dry run reports) and release.sh (except
# `check`, which reports), next to scripts/cve-gate.sh.
#
# WHY (2026-11-02, #74): 1.62.0 shipped every image on the bases pinned
# on 2026-10-19 (wolfi-base built 2026-10-17), at least four days after
# Chainguard published bases that fixed a critical CVE in
# ca-certificates-bundle and a high one in brotli (wolfi-base built
# 2026-10-28). The pins are digests, so a rebuild changes nothing, and
# `apk add` installs what a base lacks but never upgrades what it has.
# docs/chainguard-migration.md said each release refreshes the pins;
# nothing made a release do it, and cve-gate.sh reads only the Cargo.lock
# files, never the images' OS packages.
#
# WHAT IT CHECKS: scripts/refresh-chainguard-bases.sh --check, which asks
# the registry for each pinned tag's current multi-arch index (no pull)
# and compares digests. A pin is stale the moment the tag moves on; there
# is no grace period: the registry gives no history, so it cannot say
# how long a newer base has been out, and the base that shipped 1.62.0
# had been superseded for at least four days. wolfi-base's `:latest` was
# built on 2026-10-17, 10-28 and 11-02, so refresh the pins as the LAST
# edit before the release commit (scripts/refresh-chainguard-bases.sh,
# then rebuild).
#
# REFUSALS: exit 1 from the check (a pin behind, or a base not pinned) and
# anything else (the registry did not answer, docker buildx is missing) —
# a check that could not look is not a pass. If the tag moves between the
# release commit and the publish, the pins were current when committed:
# cut the next patch with fresh pins, or set BASE_PIN_WAIVER="<why>" for
# this one run — printed, so the waiver is on the release log.
#
# Kept bash-3.2 compatible (macOS /bin/bash).
# ---------------------------------------------------------------------------
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

mode=gate
case "${1:-}" in
    "") ;;
    --report) mode=report ;;
    *) echo "usage: $0 [--report]" >&2; exit 2 ;;
esac

refuse() {
    if [ "$mode" = report ]; then
        echo "  base-pin gate (report only): $*"
        exit 0
    fi
    if [ -n "${BASE_PIN_WAIVER:-}" ]; then
        echo "  base-pin gate WAIVED: $* — waiver: $BASE_PIN_WAIVER"
        exit 0
    fi
    echo "REFUSING: $*" >&2
    echo "  Refresh them (scripts/refresh-chainguard-bases.sh), rebuild and" >&2
    echo "  commit, or set BASE_PIN_WAIVER=\"<why>\" for this one run." >&2
    exit 1
}

echo "  base-pin gate: Chainguard base pins against the registry"
set +e
"$here/refresh-chainguard-bases.sh" --check
rc=$?
set -e
# refresh-chainguard-bases.sh --check: 0 = every pin current; 1 = a pin
# behind or a base unpinned; 2 (or anything else) = it could not answer.
case "$rc" in
    0) echo "  base-pin gate: every Chainguard base pin is current" ;;
    1) refuse "Chainguard base pins above are behind their tags or unpinned" ;;
    *) refuse "refresh-chainguard-bases.sh --check exited $rc — the registry was not asked or did not answer" ;;
esac
