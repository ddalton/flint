#!/usr/bin/env bash
# Run a SUBSET of run-s3csi.sh's legs against a rig its `setup` built:
#
#   CTX=kind-flint-s3csi ./run-legs.sh S23 S24
#
# The drill's knobs and helpers are imported verbatim (the trick
# aws-passthrough.sh uses), then each named leg's block — from its
# `# ── <ID>` header to the next header — is evaluated in the order
# asked. The roster is the legs asked for, so a leg that is not in the
# drill, or that dies before its `leg` line, fails the run. Legs that
# depend on an earlier leg's state (S17f on S17f-seed, S8 collect on
# S8) must be asked for together, in order.
set -u
cd "$(dirname "$0")"
[ $# -gt 0 ] || { echo "usage: $0 LEG [LEG...]" >&2; exit 2; }
REPO=$(cd ../.. && pwd)
eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
for id in "$@"; do
    block=$(awk -v id="$id" '
        /^# ── / { on = ($0 ~ ("^# ── " id "( |$)")) }
        on { print }' run-s3csi.sh)
    [ -n "$block" ] || { bad "leg $id: no '# ── $id' header in run-s3csi.sh"; continue; }
    eval "$block"
done
echo
for want in "$@"; do
    echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"
done
echo "════════════════════════════════════════"
echo "s3.csi.chert.us legs [$*]: $PASS ok, $FAILED bad"
[ "$FAILED" = "0" ]
