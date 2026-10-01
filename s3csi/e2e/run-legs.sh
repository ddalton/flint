#!/usr/bin/env bash
# Run a SUBSET of run-s3csi.sh's legs against a rig its `setup` built:
#
#   CTX=kind-flint-s3csi ./run-legs.sh S23 S24
#   SUITE=aws-passthrough.sh ./run-legs.sh P8      # another suite's legs
#
# The drill's knobs and helpers are imported verbatim (the trick
# aws-passthrough.sh uses), then each named leg's block — from its
# `# ── <ID>` header to the next header — is evaluated in the order
# asked. The roster is the legs asked for, so a leg that is not in the
# drill, or that dies before its `leg` line, fails the run. Legs that
# depend on an earlier leg's state (S17f on S17f-seed, S8 collect on
# S8) must be asked for together, in order. A leg whose header appears
# twice (P8: started first, collected last) runs both blocks, in file
# order.
#
# SUITE names another drill in this directory whose legs are shaped the
# same way (aws-passthrough.sh). Its preamble — everything from its
# `set -u` to its first leg header: the env it demands, run-s3csi.sh's
# helpers, its own, its CRs applied — is evaluated first in place of the
# default import, so the leg finds what it would in a full run.
set -u
cd "$(dirname "$0")"
[ $# -gt 0 ] || { echo "usage: $0 LEG [LEG...]" >&2; exit 2; }
REPO=$(cd ../.. && pwd)
SUITE=${SUITE:-run-s3csi.sh}
[ -f "$SUITE" ] || { echo "no suite $SUITE here" >&2; exit 2; }
if [ "$SUITE" = run-s3csi.sh ]; then
    eval "$(sed -n '/^CTX=\${CTX:-/,/^# ── setup \/ teardown/p' run-s3csi.sh | sed '$d')"
else
    eval "$(awk '/^set -u/ { on = 1 } /^# ── / { exit } on { print }' "$SUITE")"
fi
for id in "$@"; do
    block=$(awk -v id="$id" '
        /^# ── / { on = ($0 ~ ("^# ── " id "( |$)")) }
        on { print }' "$SUITE")
    [ -n "$block" ] || { bad "leg $id: no '# ── $id' header in $SUITE"; continue; }
    eval "$block"
done
echo
for want in "$@"; do
    echo " $RAN_LEGS " | grep -q " $want " || bad "leg $want never ran"
done
echo "════════════════════════════════════════"
echo "s3.csi.chert.us legs [$*] of $SUITE: $PASS ok, $FAILED bad"
[ "$FAILED" = "0" ]
