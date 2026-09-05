#!/usr/bin/env bash
# C5 — a foreign DELETE of an object the export's manifest cites.
#
# A read-write passthrough mount carries `--allow-delete`
# (mounter.rs:113-116), so deletion is inside what a second writer on
# prefix B can do. C3 covers an overwrite; a delete is the other half,
# and it is the worse one for a reader that fetches by key: an
# overwrite yields wrong bytes, a delete yields nothing at all.
#
# Two questions:
#   1. Does the lean reader fail closed on a MISSING cited object, the
#      way it does on a changed one?
#   2. Does any later export put it back? The export uploads what its
#      local scan says CHANGED; an object deleted behind its back
#      changed nothing locally, so the prediction is no.
#
# ANTI-VACUITY. The object is confirmed present AND confirmed cited by
# the live manifest before it is deleted — deleting something the
# manifest never mentioned would prove nothing about citation.
#
#   bash forge/e2e/composition/c5-cited-delete.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c5}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
rig_purge c5/

A=c5/A; B=c5/B
head_ "setup"
new_bare_repo "$WORK/A.git"
forge_up A "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
wait_key "$A/git/epoch" 30 >/dev/null
new_clone "$WORK/A.git" "$WORK/wc"
printf 'readme\n' > "$WORK/wc/README.md"
mkdir -p "$WORK/wc/src"; printf 'fn main() { }\n' > "$WORK/wc/src/main.rs"
git_c "$WORK/wc" add README.md src/main.rs
git_c "$WORK/wc" commit -qm one
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
wait_key "$B/files/src/main.rs" 45 && ok "the export published src/main.rs" || bad "no export"

head_ "preconditions"
s3_has "$B/files/src/main.rs" && ok "the object exists" || bad "the object is missing already"
# Cited: a fresh lean checkout of the UNDAMAGED prefix must succeed and
# produce the file. That is what makes the manifest's citation real.
mkdir -p "$WORK/pre"
forge_down A; sleep 2
lean checkout "$B" "$WORK/pre" >/dev/null 2>&1
[ -f "$WORK/pre/src/main.rs" ] \
  && ok "the manifest cites it (a clean checkout materialises it)" \
  || bad "precondition FAILED: the manifest does not cite it"

head_ "the foreign delete"
s3_rm "$B/files/src/main.rs"
s3_has "$B/files/src/main.rs" && bad "the delete did not take" || ok "the object is gone"

head_ "C5 — the lean reader on a missing cited object"
mkdir -p "$WORK/reader"
out=$(lean checkout "$B" "$WORK/reader"); rc=$?
if [ $rc -ne 0 ]; then
  ok "the lean reader REFUSED (rc=$rc)"
  note "$(printf '%s' "$out" | tail -2 | head -1)"
else
  bad "the lean reader accepted a manifest citing a missing object (rc=0)"
fi
[ -f "$WORK/reader/src/main.rs" ] \
  && bad "it materialised the file anyway" \
  || ok "it did not invent the missing file"

head_ "C5b — does a later export restore it?"
forge_up A2 "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
wait_key "$A/git/epoch" 40 >/dev/null
FORGE_SOCKET=/tmp/fc-A2.sock
printf 'readme two\n' >> "$WORK/wc/README.md"
git_c "$WORK/wc" commit -qam two
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
sleep 15
if s3_has "$B/files/src/main.rs"; then
  stale A3 "a later export restored the deleted object"
else
  accepted A3 "the deleted object is not restored — the export uploads only what changed locally"
fi
forge_down A2
verdict "C5"
