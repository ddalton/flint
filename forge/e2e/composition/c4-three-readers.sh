#!/usr/bin/env bash
# C4 — reading a diverged export prefix three ways.
#
# C3 establishes that a foreign write into prefix B survives every
# later export. This drill asks the question that decides how bad that
# is: WHO NOTICES?
#
# The design's composition puts three kinds of reader on prefix B, and
# the first draft of this drill predicted they would split: lean reads
# the manifest and fetches each object AT THE ETAG THE MANIFEST CITES
# (checkout.rs:257), so it looked certain to fail closed where a
# manifest-less reader would not.
#
# THAT PREDICTION IS WRONG, AND THE DRILL IS WHAT SHOWED IT. The loud
# refusal on a changed object is guarded by `if pinned`
# (checkout.rs:258) — it fires only under a GATED citation. For the
# cadence/hybrid/legacy manifests the export actually writes, the next
# arm takes over (checkout.rs:279-291) and is explicit about what it
# does: "S3-wins: the object moved past the manifest ... Adopt the
# CURRENT version." That is right for lean's own workspace, where the
# object moving past the manifest means a human wrote newer bytes. On
# an export prefix, where forge is the only legitimate writer, the
# same sentence means somebody wrote who should not have — and the
# reader adopts it.
#
# So the interesting result is not a split but its absence: on an
# OVERWRITE every reader is fooled, and lean is fooled hardest, because
# it copies the foreign bytes into an agent's workspace where they can
# be committed back into git as though they were real. (C5 covers the
# other half: a DELETE is refused loudly, by the very next match arm.)
#
# WHAT THIS DRILL SUBSTITUTES. The passthrough/lite reader is a plain
# S3 GET. Mountpoint does not run on this host, and what is under test
# is whether a manifest-less read of a key returns the foreign bytes
# without complaint — which is a property of the key, not of the FUSE
# layer above it.
#
#   bash forge/e2e/composition/c4-three-readers.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c4}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
rig_purge c4/

A=c4/A; B=c4/B
head_ "setup — publish an export, then diverge one object"
new_bare_repo "$WORK/A.git"
forge_up A "$WORK/A.git" "$A" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=$B" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
wait_key "$A/git/epoch" 30 >/dev/null
new_clone "$WORK/A.git" "$WORK/wc"
printf 'the real readme\n' > "$WORK/wc/README.md"
mkdir -p "$WORK/wc/src"; printf 'fn main() { }\n' > "$WORK/wc/src/main.rs"
git_c "$WORK/wc" add README.md src/main.rs
git_c "$WORK/wc" commit -qm one
push "$WORK/wc" HEAD:refs/heads/main >/dev/null 2>&1
wait_key "$B/files/README.md" 45 && ok "the export published" || bad "no export"

printf 'FOREIGN BYTES - not in any commit\n' > "$WORK/evil"
s3_put "$B/files/README.md" "$WORK/evil"
[ "$(s3_cat "$B/files/README.md")" != "the real readme" ] \
  && ok "precondition: the prefix is diverged" \
  || bad "precondition FAILED: the prefix is not diverged"

# Stop forge so the read legs are not racing an export for B's lease.
forge_down A
sleep 2

# ── reader 1: by key, no manifest (passthrough / lite) ───────────────
head_ "reader 1 — by key, no manifest (passthrough / lite)"
got=$(s3_cat "$B/files/README.md")
if [ "$got" = "the real readme" ]; then
  ok "the key holds the committed bytes"
else
  bad "a manifest-less reader is served bytes from no commit, with no error"
  note "it read: $got"
fi

# ── reader 2: lean, which reads the manifest ─────────────────────────
head_ "reader 2 — lean checkout, which reads the manifest"
mkdir -p "$WORK/reader"
out=$(lean checkout "$B" "$WORK/reader"); rc=$?
if [ $rc -ne 0 ]; then
  ok "the lean reader REFUSED the diverged prefix (rc=$rc)"
  note "$(printf '%s' "$out" | grep -i 'etag\|cites' | head -2)"
else
  bad "the lean reader accepted the diverged prefix (rc=0)"
fi
if [ -f "$WORK/reader/README.md" ]; then
  if [ "$(cat "$WORK/reader/README.md")" = "the real readme" ]; then
    ok "the lean reader materialised the committed bytes"
  else
    bad "the lean reader materialised the FOREIGN bytes into a workspace"
  fi
else
  ok "the lean reader materialised nothing rather than materialising a lie"
fi

# ── reader 3: git, the source of truth ───────────────────────────────
head_ "reader 3 — git clone, which never touches the export"
git clone -q "$WORK/A.git" "$WORK/gitreader" 2>/dev/null
if [ "$(cat "$WORK/gitreader/README.md" 2>/dev/null)" = "the real readme" ]; then
  ok "git still serves the committed bytes (the repository itself is intact)"
else
  bad "the repository itself is damaged"
fi

head_ "C4 — the split"
note "one corrupt prefix: git intact; every reader of the export takes the foreign bytes"
note "lean adopts them into a workspace (checkout.rs:279-291, the unpinned S3-wins arm)"
verdict "C4"
