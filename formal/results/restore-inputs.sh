#!/usr/bin/env bash
# restore-inputs.sh: rebuild a results batch's input tree as it ran, from its INPUTS.tsv.
#
#   restore-inputs.sh BATCH_DIR --check    check every row: a "here" file and every copy's
#                                          source exist in the repo with the listed md5
#   restore-inputs.sh BATCH_DIR OUT_DIR    copy the batch to OUT_DIR (outside the repo), put
#                                          each deduplicated file back at its path from its
#                                          source, and check every md5
#
# Why: a batch's job and stage directories held many byte-identical copies of the same
# model, proof and cfg.  The repo keeps each once; INPUTS.tsv lists every input file as
# it ran (md5, path in the batch, and "here", the repo path of the kept copy, or
# "absent: <why>").  Restore into a directory outside the repo, so the copies never get
# committed back.
set -euo pipefail
batch=$(cd "${1:?usage: restore-inputs.sh BATCH_DIR --check|OUT_DIR}" && pwd)
mode=${2:?usage: restore-inputs.sh BATCH_DIR --check|OUT_DIR}
root=$(git -C "$batch" rev-parse --show-toplevel)
tsv=$batch/INPUTS.tsv
[ -f "$tsv" ] || { echo "restore-inputs: no INPUTS.tsv in $batch" >&2; exit 2; }
md5of() { if command -v md5sum >/dev/null 2>&1; then md5sum < "$1" | cut -d' ' -f1; else md5 -q "$1"; fi; }

if [ "$mode" != --check ]; then
    out=$mode
    case "$(cd "$(dirname "$out")" 2>/dev/null && pwd)/" in
        "$root"/*) echo "restore-inputs: OUT_DIR must be outside the repo ($root)" >&2; exit 2;;
    esac
    [ -e "$out" ] && { echo "restore-inputs: $out exists; give a new directory" >&2; exit 2; }
    mkdir -p "$out"
    cp -R "$batch/." "$out/"
fi

rows=0 ok=0 absent=0 bad=0
while IFS=$'\t' read -r want rel src; do
    case "$want" in '#'*|'') continue;; esac
    rows=$((rows + 1))
    case "$src" in
        absent:*) absent=$((absent + 1)); continue;;
        here) from=$batch/$rel;;
        *) from=$root/$src;;
    esac
    if [ ! -f "$from" ]; then echo "MISSING $rel (from $from)" >&2; bad=$((bad + 1)); continue; fi
    if [ "$mode" = --check ]; then
        f=$from
    else
        f=$out/$rel
        mkdir -p "$(dirname "$f")"
        [ "$src" = here ] || cp "$from" "$f"
    fi
    got=$(md5of "$f")
    if [ "$got" = "$want" ]; then ok=$((ok + 1)); else echo "MD5 MISMATCH $rel: $got, want $want" >&2; bad=$((bad + 1)); fi
done < "$tsv"
echo "restore-inputs: $(basename "$batch"): $rows rows, $ok md5-checked, $absent absent (not in the repo), $bad bad"
[ "$bad" -eq 0 ]
