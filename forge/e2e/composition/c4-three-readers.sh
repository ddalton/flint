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
# THAT PREDICTION WAS WRONG, AND THE DRILL IS WHAT SHOWED IT. The loud
# refusal on a changed object was guarded by `if pinned` — it fired
# only under a GATED citation. For the cadence/hybrid manifests the
# export actually writes, the next arm took over and was explicit about
# what it did: "S3-wins: the object moved past the manifest ... Adopt
# the CURRENT version." Right for a lean workspace, where that means a
# human wrote newer bytes. On an export prefix, where forge is the only
# legitimate writer, it means somebody wrote who should not have — and
# lean copied the foreign bytes into an agent's workspace and reported
# success.
#
# THE FIX. A manifest now carries `sole_writer`, set by the installing
# pass from config, and forge's export sets it (`FLINT_SYNC_SOLE_WRITER`
# in `barrier_command`). A reader that finds an object off its citation
# in such a workspace REFUSES instead of adopting. The flag lives in
# the manifest rather than in the reader's config on purpose: a reader
# that has to be configured to be careful is one that will eventually
# be deployed without it.
#
# WHAT THE FIX DOES NOT REACH, and cannot. Reader 1 below has no
# manifest — it is a key and a GET. There is nothing to check the bytes
# against, so it still takes the foreign write, and that leg still
# FAILS. That is the honest boundary of a mirror: readers that verify
# can be told, readers that do not verify cannot. It is an argument for
# C3 (repair the divergence at the source), not for more reader code.
#
# Note also that the flag only protects a manifest PUBLISHED with it: an
# export written before this change stays adoptable until it publishes
# again.
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
  printf '%s' "$out" | grep -o 'SOLE WRITER' >/dev/null \
    && ok "the refusal says the workspace has one writer" \
    || bad "it refused, but not for this reason"
  printf '%s' "$out" | grep -q 'recover-staged' \
    && bad "it gave the gated lane's advice; nothing was staged here" \
    || ok "it did not give the wrong remedy"
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
note "git intact; lean now refuses; a manifest-less reader still takes the bytes"
note "the last one is not fixable at the reader — it is the argument for repairing C3 at the source"
verdict "C4"
