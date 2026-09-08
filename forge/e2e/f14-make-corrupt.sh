#!/usr/bin/env bash
# Manufacture an unrestorable bucket from a HEALTHY one, and prove it is
# unrestorable before handing it to anything.
#
#   BUCKET=... SRC=drill/f14 OUT=/tmp/corrupt ./forge/e2e/f14-make-corrupt.sh
#   then: CORRUPT_DIR=/tmp/corrupt ./forge/e2e/f14-corrupt-control.sh
#
# WHY THIS EXISTS. `f14-corrupt-control.sh` takes CORRUPT_DIR — the
# preserved prefix of a run that produced the state. runcj's is gone
# with the session that made it, so the control could not be run again
# at all. A positive control that depends on a one-off artifact is not
# a control you can rely on; this makes the recipe reproducible.
#
# WHAT IT DOES NOT CLAIM. This is not runcj's bytes. It reproduces the
# SHAPE — `broken link ... missing commit`, the tip present and an
# ancestor absent — which is what the oracle must discriminate.
#
# THE PACK IS CHOSEN BY MEASUREMENT, NOT BY REASONING, and that is the
# whole point of this script. The obvious pick — "the oldest named pack
# holds the ancestry" — is WRONG once compaction has folded: on runcl,
# two of the four packs the snapshot named were redundant, their objects
# already copied into a later pack. Removing one changed nothing, the
# repository came up Ready, and the control reported the ORACLE as blind
# when the fault was the corruption. So every candidate is tested with
# the same `fsck --connectivity-only` the syncer runs, and one is chosen
# only if dropping it actually breaks the chain — and breaks it in the
# right way: `broken link`, not `invalid sha1 pointer`, which is the
# shape you get from dropping the pack that holds the TIP.
set -euo pipefail
: "${BUCKET:?set BUCKET}"; : "${SRC:?set SRC to a healthy <prefix>/<repo>}"
OUT=${OUT:?set OUT to the directory to build}
REF=${REF:-refs/heads/agents}

rm -rf "$OUT"; mkdir -p "$OUT"
aws s3 sync "s3://$BUCKET/$SRC/" "$OUT/" --quiet
[ -f "$OUT/git/snapshot" ] || { echo "no git/snapshot under $SRC — is that a repository prefix?" >&2; exit 2; }

SRCG="$OUT/git"
TIP=$(python3 -c "import json;print(json.load(open('$SRCG/snapshot'))['refs']['$REF'])")
read -r -a PACKS <<< "$(python3 -c "import json;print(' '.join(json.load(open('$SRCG/snapshot'))['packs']))")"
echo "snapshot names ${#PACKS[@]} pack(s); $REF = $TIP"

probe=$(mktemp -d)
build() { local d=$1; shift; rm -rf "$d"; git init -q --bare "$d"
  local p stem
  for p in "$@"; do stem=${p%.pack}
    cp "$SRCG/objects/pack/$stem.pack" "$SRCG/objects/pack/$stem.idx" "$d/objects/pack/"; done
  printf '%s\n' "$TIP" > "$d/$REF"; }

# The baseline must be HEALTHY, or "dropping X breaks it" means nothing.
build "$probe/all" "${PACKS[@]}"
if ! git --git-dir="$probe/all" fsck --connectivity-only >/dev/null 2>&1; then
  echo "the SOURCE bucket is already unrestorable — nothing to manufacture" >&2; exit 2
fi
echo "baseline: the source restores cleanly"

VICTIM=""
for cand in "${PACKS[@]}"; do
  keep=(); for p in "${PACKS[@]}"; do [ "$p" = "$cand" ] || keep+=("$p"); done
  [ ${#keep[@]} -gt 0 ] || continue
  build "$probe/x" "${keep[@]}"
  out=$(git --git-dir="$probe/x" fsck --connectivity-only 2>&1) && { echo "  ${cand:5:12}: redundant (folded elsewhere)"; continue; }
  if printf '%s' "$out" | grep -q 'broken link'; then
    echo "  ${cand:5:12}: LOAD-BEARING, and breaks the chain the right way"
    VICTIM=$cand; break
  fi
  echo "  ${cand:5:12}: load-bearing but drops the tip (invalid sha1) — wrong shape, keeping"
done
rm -rf "$probe"
[ -n "$VICTIM" ] || { echo "no pack yields 'broken link' — cannot manufacture this shape here" >&2; exit 1; }

python3 - "$OUT" "$VICTIM" <<'PY'
import json, os, sys
root, victim = sys.argv[1], sys.argv[2]
p = os.path.join(root, "git", "snapshot")
d = json.load(open(p)); d["packs"] = [x for x in d["packs"] if x != victim]
json.dump(d, open(p, "w"))
pdir = os.path.join(root, "git", "objects", "pack"); stem = victim[:-5]
for ext in (".pack", ".idx", ".rev"):
    f = os.path.join(pdir, stem + ext)
    if os.path.exists(f): os.remove(f)
PY
# Removed from the LIST as well as the tree: leaving it listed gives a
# missing-pack download error, which is a different refusal from the
# `fsck --connectivity-only` broken link the oracle is meant to catch.
echo "built $OUT — dropped $VICTIM from the tree AND from the snapshot's pack list"
