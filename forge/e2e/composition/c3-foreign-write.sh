#!/usr/bin/env bash
# C3 — a foreign write into forge's export prefix.
#
# THE CLAIM UNDER TEST is forge's own, written at export.rs:27:
#
#     "The export is a MIRROR, never a source of truth. A foreign write
#      into its prefix is overwritten by the next export, and the CRD
#      says so."
#
# That is the sentence that makes a read-write passthrough mount over
# prefix B sound survivable: whatever it writes, the next export puts
# back. A passthrough mount in read-write mode is a real second writer
# — `mounter.rs:113-116` passes `--allow-delete --allow-overwrite` —
# so the sentence is load-bearing, not hypothetical.
#
# WHY IT MIGHT NOT HOLD. The export's barrier computes what to upload
# by diffing a LOCAL scan against a LOCAL baseline (barrier.rs:469-475).
# The only remote thing it consults is the manifest pointer's etag. A
# foreign write to a data object moves no pointer and changes no local
# file, so there is nothing in that diff to notice it.
#
# WHAT THIS DRILL SUBSTITUTES. The foreign writer here is a plain S3
# PUT, not a Mountpoint process. That is faithful to the claim under
# test — what is at stake is whether the EXPORT notices a changed
# object, and the identity of whoever changed it does not enter the
# computation. A real passthrough mount would additionally choose its
# own part size and etag shape; that is not what is being measured.
#
# ANTI-VACUITY. Two preconditions are asserted before the finding:
# the foreign write must actually change the object's etag (or there is
# nothing to detect), and the next export must actually RUN and change
# something else (or "it did not repair README" only says it was idle).
#
#   bash forge/e2e/composition/c3-foreign-write.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c3}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
rig_purge c3/

A=c3/A; B=c3/B
head_ "setup — forge on $A exporting to $B"
new_bare_repo "$WORK/A.git"
forge_up A "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
wait_key "$A/git/epoch" 30 && ok "forge holds its lease" || bad "no lease"

new_clone "$WORK/A.git" "$WORK/wc"
printf 'the real readme\n' > "$WORK/wc/README.md"
mkdir -p "$WORK/wc/src"; printf 'fn main() { }\n' > "$WORK/wc/src/main.rs"
git_c "$WORK/wc" add README.md src/main.rs
git_c "$WORK/wc" commit -qm one
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
wait_key "$B/files/README.md" 45 && ok "the export published README.md" || bad "no export"

orig_etag=$(s3_etag "$B/files/README.md")
[ "$(s3_cat "$B/files/README.md")" = "the real readme" ] \
  && ok "the export's README.md holds the committed bytes" \
  || bad "the export did not publish the committed bytes"

# ── the foreign write ────────────────────────────────────────────────
head_ "the foreign write — a second writer overwrites one exported object"
printf 'FOREIGN BYTES - not in any commit\n' > "$WORK/evil"
s3_put "$B/files/README.md" "$WORK/evil"
new_etag=$(s3_etag "$B/files/README.md")
note "etag $orig_etag -> $new_etag"
if [ -n "$new_etag" ] && [ "$new_etag" != "$orig_etag" ]; then
  ok "precondition: the foreign write changed the object's etag"
else
  bad "precondition FAILED: the etag did not move, so there is nothing to detect"
fi

# ── the next export ──────────────────────────────────────────────────
head_ "the next export — it runs, and it changes a DIFFERENT file"
printf 'fn main() { println!("two"); }\n' > "$WORK/wc/src/main.rs"
git_c "$WORK/wc" commit -qam two
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
for _ in $(seq 1 45); do
  [ "$(s3_cat "$B/files/src/main.rs")" = 'fn main() { println!("two"); }' ] && break
  sleep 1
done
if [ "$(s3_cat "$B/files/src/main.rs")" = 'fn main() { println!("two"); }' ]; then
  ok "precondition: the export RAN and republished the file git changed"
else
  bad "precondition FAILED: the export did not run, so nothing below is evidence"
fi

head_ "C3 — did the export overwrite the foreign write?"
now=$(s3_cat "$B/files/README.md")
note "README.md in the export now reads: $now"
if [ "$now" = "the real readme" ]; then
  ok "the export overwrote the foreign write, as export.rs:27 claims"
else
  bad "the export did NOT overwrite the foreign write — the mirror is serving bytes from no commit"
fi

# And is it self-healing on a later pass, or permanent?
head_ "C3b — is the divergence permanent?"
printf 'three\n' >> "$WORK/wc/src/main.rs"
git_c "$WORK/wc" commit -qam three
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
sleep 12
now2=$(s3_cat "$B/files/README.md")
if [ "$now2" = "the real readme" ]; then
  ok "a later export repaired it"
else
  bad "still diverged after a further export — nothing in the export path ever repairs it"
fi

# The manifest still attests to the commit, which is what makes it quiet.
head_ "C3c — what the export's own pointer says"
ec=$(s3_cat "$A/git/snapshot" | sed 's/.*"exported_commit":"\([^"]*\)".*/\1/')
note "the snapshot's exported_commit = ${ec:-<none>}"
note "a reader trusting that pointer believes README.md is that commit's content"

forge_down A
verdict "C3"
