#!/usr/bin/env bash
# tlapm-rerun.sh: gate item 2's loose end. Rerun a control's failed proof steps alone and with
# more time, so a "killed" step can't be a solver timeout under load (the 2026-11-07 controls
# ran on shared nodes and took tlapm's "failed" as reported).
#
#   ./tlapm-rerun.sh CONTROL LINE...   e.g. ./tlapm-rerun.sh K-MonitorGuard 3639
#   ./tlapm-rerun.sh CONTROL all       every step CONTROL failed on 2026-11-07 (controls tables.json)
#   ./tlapm-rerun.sh BASE LINE...      the unflipped proof, where every step must prove
#
# For each LINE: a fresh directory with the proof, Ind and model copied from formal/ (md5s
# asserted: 9ed5580a / c70f9660 / 61ae6829), the control's diff applied
# (../2026-11-07-controls/diffs/CONTROL.diff), the flipped proof's md5 asserted against the one
# the controls table recorded, then
#   tlapm --toolbox LINE LINE --cleanfp --stretch STRETCH --threads THREADS FlintCatchupSProof.tla
# and every obligation that starts on LINE is tallied (formal/sproof/tlapm-tally.py).
#
# Environment: TLAPM (default: tlapm on PATH; the record's build is tlapm bfa9468), STRETCH
# (default 20; the record and the controls used 5), THREADS (default 8), DRY=1 (prepare and
# assert, print the tlapm command, don't run it).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
formal=$(cd "$here/../.." && pwd)
controls=$formal/results/2026-11-07-controls
ctl=${1:?usage: tlapm-rerun.sh CONTROL|BASE LINE...|all}
shift
[ $# -gt 0 ] || { echo "tlapm-rerun: give LINE... or all" >&2; exit 2; }
TLAPM=${TLAPM:-tlapm}
STRETCH=${STRETCH:-20}
THREADS=${THREADS:-8}
md5of() { if command -v md5sum >/dev/null 2>&1; then md5sum < "$1" | cut -c1-8; else md5 -q "$1" | cut -c1-8; fi; }

# the flipped proof's md5 and the failed steps, as the controls table recorded them
read -r want_proof failed <<EOF
$(python3 -I - "$controls/tables.json" "$ctl" <<'PY'
import json, sys
t = json.load(open(sys.argv[1]))
c = t.get(sys.argv[2])
if c is None:
    sys.exit("tlapm-rerun: no control %s in tables.json (%s)" % (sys.argv[2], ", ".join(sorted(t))))
m = c["md5"][0]["FlintCatchupSProof.tla"][:8]
print(m, ",".join(sorted({str(f[0]) for f in c["fails"]}, key=int)) or "-")
PY
)
EOF
[ -n "$want_proof" ] || exit 2
lines=("$@")
if [ "${lines[0]}" = all ]; then
    [ "$failed" != - ] || { echo "tlapm-rerun: $ctl failed no step on 2026-11-07" >&2; exit 2; }
    IFS=, read -r -a lines <<< "$failed"
fi

mkdir -p "$here/logs" "$here/work"
if [ -z "${DRY:-}" ]; then
    tlver=$("$TLAPM" --version 2>&1 | head -1)
else
    tlver="(dry run)"
fi
for L in "${lines[@]}"; do
    w=$here/work/tlapm-$ctl-$L
    rm -rf "$w" && mkdir -p "$w"
    for f in FlintCatchupSProof.tla:9ed5580a FlintCatchupSInd.tla:c70f9660 FlintCatchupS.tla:61ae6829; do
        p=${f%%:*} want=${f##*:}
        got=$(md5of "$formal/$p")
        [ "$got" = "$want" ] || { echo "tlapm-rerun: formal/$p is $got, want $want" >&2; exit 1; }
        cp "$formal/$p" "$w/"
    done
    if [ "$ctl" != BASE ]; then
        (cd "$w" && patch -p1 --quiet < "$controls/diffs/$ctl.diff")
    fi
    got=$(md5of "$w/FlintCatchupSProof.tla")
    [ "$got" = "$want_proof" ] || { echo "tlapm-rerun: $ctl's proof is $got after the diff, the table recorded $want_proof" >&2; exit 1; }
    out=$here/logs/tlapm-$ctl-$L.out
    cmd=("$TLAPM" --toolbox "$L" "$L" --cleanfp --stretch "$STRETCH" --threads "$THREADS" FlintCatchupSProof.tla)
    echo "TLAPM $ctl line $L | proof $got (as recorded) ind c70f9660 model 61ae6829 | $tlver | ${cmd[*]} | failed on 2026-11-07 at lines: $failed" | tee -a "$here/runs.out"
    [ -n "${DRY:-}" ] && continue
    t0=$(date +%s)
    set +e
    (cd "$w" && nice -n 5 "${cmd[@]}") > "$out" 2>&1
    rc=$?
    set -e
    tally=$(python3 -I "$formal/sproof/tlapm-tally.py" "$out")
    onl=$(echo "$tally" | awk -v L="$L" '$1 == "NOT" && $3 ~ "^" L ":" {print $3, $4}' | tr '\n' ' ')
    verdict=PROVED
    [ -n "$onl" ] && verdict="NOT PROVED: $onl"
    echo "DONE tlapm $ctl line $L stretch $STRETCH: $verdict rc=$rc wall=$(( $(date +%s) - t0 ))s | $(echo "$tally" | head -1)" | tee -a "$here/runs.out"
done
