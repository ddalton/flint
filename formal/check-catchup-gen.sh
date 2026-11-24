#!/usr/bin/env bash
# check-catchup-gen.sh: regenerate every FlintCatchupS revision and its cfgs from the kept
# generators and check them byte for byte against the files the batches ran.
#
#   formal/check-catchup-gen.sh
#
# The generators are kept exactly as they ran, so they still read and write
# ~/github/flint-rebuild-proof/formal (the workstation that ran them) and overwrite
# FlintCatchupS.tla there. This script never lets them touch the repo: each runs with HOME set
# to a fresh temporary directory that holds only a copy of FlintCatchupT.tla (rev 15,
# 8ac3cf00), the input every model generator derives from.
set -euo pipefail
formal=$(cd "$(dirname "$0")" && pwd)
md5of() { if command -v md5sum >/dev/null 2>&1; then md5sum < "$1" | cut -c1-8; else md5 -q "$1" | cut -c1-8; fi; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/catchup-gen.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
[ "$(md5of "$formal/FlintCatchupT.tla")" = 8ac3cf00 ] || { echo "FlintCatchupT.tla is not rev 15 (8ac3cf00)" >&2; exit 1; }
bad=0

# rev -> the model md5 the tables cite; rev 17 is formal/FlintCatchupS.tla itself
models="r8:ab167ffe r9:c41c6986 r10:2a9567c7 r11:46c89207 r12:8e54566d r13:41fa0652 r14:231150e6 r15:3d409f1b r16:d9062ab6 cur:61ae6829"
for rm in $models; do
    r=${rm%%:*} want=${rm##*:}
    g=gen-FlintCatchupS-$r.py; [ "$r" = cur ] && g=gen-FlintCatchupS.py
    h=$tmp/m-$r; d=$h/github/flint-rebuild-proof/formal
    mkdir -p "$d"; cp "$formal/FlintCatchupT.tla" "$d/"
    (cd "$h" && HOME=$h python3 -I "$formal/$g" > /dev/null)
    got=$(md5of "$d/FlintCatchupS.tla")
    if [ "$got" = "$want" ]; then echo "model $r: $got ok"; else echo "model $r: $got, want $want" >&2; bad=1; fi
done
[ "$(md5of "$formal/FlintCatchupS.tla")" = 61ae6829 ] || { echo "formal/FlintCatchupS.tla is not rev 17 (61ae6829)" >&2; bad=1; }

# every cfg a generator writes must be byte-identical to a cfg some batch ran: the as-run record
# (results/2026-10-29-flintcatchupt/as-run/MD5SUMS), or the copy of the same name in formal/
asrun=$formal/results/2026-10-29-flintcatchupt/as-run/MD5SUMS
for r in r8 r9 r10 r11 r12 r15 r16 cur; do
    g=gen-FlintCatchupS-cfg-$r.py; [ "$r" = cur ] && g=gen-FlintCatchupS-cfg.py
    h=$tmp/c-$r; d=$h/github/flint-rebuild-proof/formal
    mkdir -p "$d"
    (cd "$h" && HOME=$h python3 -I "$formal/$g" > /dev/null)
    n=0 ok=0
    for c in "$d"/*.cfg; do
        n=$((n + 1)); m=$(md5of "$c"); b=$(basename "$c")
        if [ -f "$formal/$b" ] && [ "$(md5of "$formal/$b")" = "$m" ]; then ok=$((ok + 1))
        elif grep -q "^$m" "$asrun"; then ok=$((ok + 1))
        else echo "cfg $r: $b ($m) matches no cfg that ran" >&2; fi
    done
    [ "$ok" = "$n" ] || bad=1
    echo "cfgs $r: $ok of $n match a cfg that ran"
done
[ "$bad" = 0 ] && echo "check-catchup-gen: all reproduced" || { echo "check-catchup-gen: MISMATCH" >&2; exit 1; }
