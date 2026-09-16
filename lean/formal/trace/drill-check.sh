#!/usr/bin/env bash
# W4 phase 2: a LIVE DRILL leg, projected onto one path, must be a behaviour
# of LeanSubtree.tla — and a corrupted projection must not be.
#
#   drill-check.sh <collect-dir> <path> [<path> ...]
#
# Accepted = TLC violates TraceIncomplete (it followed the trace to the end).
# Rejected = TLC exhausts short of it; the last TRACE-REACHED names the step
# it could not take, the writers' program counters, and the cell.
#
# The mutations below each corrupt ONE fact the projection still checks — a
# consume's adoption, a withheld citation, a GC's result — so a checker that
# accepted them would accept anything. A projected replay does NOT check the
# counts a leg takes over every path (see drill2tla.py).
set -u
cd "$(dirname "$0")"
JAR="${TLA_TOOLS_JAR:-../../../.tla2tools.jar}"
JAR="$(cd "$(dirname "$JAR")" && pwd)/$(basename "$JAR")"
WORK="${TRACE_WORK:-$(mktemp -d)}"
mkdir -p "$WORK"
COLLECT=${1:?usage: drill-check.sh <collect-dir> <path> [<path> ...]}
shift
FAIL=0

run_tlc() { # <dir> -> prints accept|reject|error
  local out=$1
  cp ../LeanSubtree.tla TraceLean.tla "$out/"
  (cd "$out" && java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers 1 \
      -metadir states -config TraceLean.cfg TraceLean.tla > tlc.log 2>&1)
  if grep -q "Invariant TraceIncomplete is violated" "$out/tlc.log"; then echo accept
  elif grep -q "Model checking completed. No error has been found" "$out/tlc.log"; then echo reject
  else echo error; fi
}

reached() { python3 -c 'import re,sys; t=open(sys.argv[1]).read(); m=re.findall(r"<<\s*\"TRACE-REACHED\",.*?>>\n", t, re.S); print(re.sub(r"\s+"," ",m[-1]).strip() if m else "")' "$1/tlc.log"; }

check() { # <ndjson> <expect> <name>
  local src=$1 want=$2 name=$3 got out
  out="$WORK/$name"
  rm -rf "$out"; mkdir -p "$out"
  if ! python3 ndjson2tla.py "$src" "$out" > "$out/convert.txt" 2>&1; then
    cat "$out/convert.txt"
    if [ "$want" = reject ]; then echo "== $name: rejected at conversion (as required)"; return; fi
    FAIL=$((FAIL + 1)); return
  fi
  got=$(run_tlc "$out")
  if [ "$got" = "$want" ]; then
    echo "== $name: $got (as required) — $(reached "$out" | cut -c1-150)"
  else
    echo "== $name: $got, but $want was required"; echo "   $(reached "$out")"; FAIL=$((FAIL + 1))
  fi
}

for P in "$@"; do
  tag=$(echo "$P" | tr '/.' '__')
  nd="$WORK/$tag.ndjson"
  if ! python3 drill2tla.py "$COLLECT" --path "$P" -o "$nd" 2>"$WORK/$tag.project.txt"; then
    echo "== $P: NOT PROJECTED — $(tail -1 "$WORK/$tag.project.txt")"; FAIL=$((FAIL + 1)); continue
  fi
  echo "-- $P: $(tail -1 "$WORK/$tag.project.txt")"
  check "$nd" accept "$tag"
  # MUTATIONS: one fact of the real projection corrupted.
  python3 - "$nd" "$WORK/$tag" <<'PY'
import json, sys
src, stem = sys.argv[1], sys.argv[2]
evs = [json.loads(l) for l in open(src) if l.strip()]

def write(name, mutate):
    out = [dict(e) for e in evs]
    if not mutate(out):
        return
    with open(f"{stem}.{name}.ndjson", "w") as f:
        for e in out:
            f.write(json.dumps(e) + "\n")

def consume_not_adopted(evs):
    """a consume that adopted the path reports it as already superseded"""
    for e in evs:
        if e.get("ev") == "consume" and e.get("action") == "adopted":
            e["action"] = "superseded"
            return True
    return False

def withheld_citation_kept(evs):
    """a citation the commit withheld is reported as still there"""
    for e in evs:
        if e.get("ev") == "observed" and e.get("still") is False:
            e["still"] = True
            return True
    return False

def gc_deleted_not_skipped(evs):
    """a GC that skipped the object reports it deleted"""
    for e in evs:
        if e.get("ev") == "gc" and e.get("result") == "skip":
            e["result"] = "deleted"
            return True
    return False

for name, fn in (("consume-not-adopted", consume_not_adopted),
                 ("withheld-citation-kept", withheld_citation_kept),
                 ("gc-deleted-not-skipped", gc_deleted_not_skipped)):
    write(name, fn)
PY
  for m in "$WORK/$tag".*.ndjson; do
    [ -f "$m" ] || continue
    case "$m" in *"$tag.ndjson") continue;; esac
    check "$m" reject "$(basename "$m" .ndjson)"
  done
done

echo "work dir: $WORK"
[ "$FAIL" -eq 0 ] && echo "drill check: ok" || { echo "drill check: $FAIL FAILED"; exit 1; }
