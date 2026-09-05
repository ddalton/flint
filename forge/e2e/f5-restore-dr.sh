#!/usr/bin/env bash
# FALSIFIER 5 — restore fidelity, and DR from the bucket alone.
#
# Deleting the pod destroys the emptyDir, so the replacement has NOTHING
# but the bucket. That is the cold restore. Then: are the refs exactly
# the snapshot's, does fsck pass, and is a clone byte-identical to what
# was there before?
#
# The DR half asks a different question — is the bucket a REAL bare git
# repository, or only something forge can read? The design claims the
# former (§3). The check pulls the prefix down with the AWS CLI and
# clones from that copy with stock git and no forge in the path at all.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }

gitq() { kubectl exec -n "$NS" deploy/forge-proj -c syncer -- git --git-dir=/repo/$NS/$REPO.git "$@"; }

echo "══ F5: cold restore + DR ══"
BEFORE=$(gitq for-each-ref --format='%(refname) %(objectname)' | sort)
echo "refs before: $(echo "$BEFORE" | wc -l | tr -d ' ')"
# A content fingerprint that does not depend on pack layout.
FP_BEFORE=$(kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  rm -rf /tmp/pre && git -c http.$DOOR/.extraHeader=\"\$H\" clone -q $DOOR/git/$NS/$REPO.git /tmp/pre
  cd /tmp/pre && git ls-tree -r HEAD | sha256sum | cut -d' ' -f1")
echo "tree fingerprint before: $FP_BEFORE"

OLD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].metadata.name}')
echo "── destroying the pod (and with it the whole local repository) ──"
kubectl delete pod -n "$NS" "$OLD" --wait=true >/dev/null
kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null
NEW=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].metadata.name}')
[ "$NEW" != "$OLD" ] && ok "a NEW pod restored from the bucket ($OLD -> $NEW)" || bad "same pod — nothing was tested"

SNAP=$(aws s3 cp "s3://$BUCKET/$PREFIX/git/git/snapshot" - 2>/dev/null \
       | jq -r '.refs | to_entries[] | "\(.key) \(.value)"' | sort)
AFTER=$(gitq for-each-ref --format='%(refname) %(objectname)' | sort)
[ "$AFTER" = "$SNAP" ] && ok "restored refs are EXACTLY the snapshot's" \
  || bad "refs differ from the snapshot: $(diff <(echo "$AFTER") <(echo "$SNAP") | head -4 | tr '\n' ' ')"
[ "$AFTER" = "$BEFORE" ] && ok "restored refs equal what was there before the kill" || bad "refs changed across the restore"

if gitq fsck --strict >/dev/null 2>&1; then ok "fsck --strict clean on the restored repository"; else bad "fsck failed after restore"; fi

FP_AFTER=$(kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  rm -rf /tmp/post && git -c http.$DOOR/.extraHeader=\"\$H\" clone -q $DOOR/git/$NS/$REPO.git /tmp/post
  cd /tmp/post && git ls-tree -r HEAD | sha256sum | cut -d' ' -f1")
[ "$FP_AFTER" = "$FP_BEFORE" ] && ok "a clone after the restore is byte-identical ($FP_AFTER)" \
  || bad "clone differs: $FP_BEFORE -> $FP_AFTER"

echo "── DR: is the bucket a real bare repository, with no forge in the path? ──"
#
# THE TRANSPORT MATTERS AND IT IS EASY TO TEST THE WRONG ONE. The bucket
# carries `info/refs` and `objects/info/packs` — the DUMB HTTP layout,
# which is exactly what §3 claims. A LOCAL clone does not read either of
# those; it reads `refs/` and `packed-refs`, neither of which is in the
# bucket. So `git clone <synced-dir>` fails, and it fails for a reason
# that says nothing about the bucket being a valid repository. Serve it
# over HTTP and stock git clones it.
TMP=$(mktemp -d)
aws s3 sync "s3://$BUCKET/$PREFIX/git/git/" "$TMP/bare.git/" --quiet
( cd "$TMP/bare.git" && python3 -m http.server 8899 >/dev/null 2>&1 & echo $! > "$TMP/srv.pid" )
sleep 2
if git -c http.followRedirects=true clone -q http://127.0.0.1:8899 "$TMP/dr" 2>/dev/null; then
  DRFP=$(cd "$TMP/dr" && git ls-tree -r HEAD | sha256sum | cut -d' ' -f1)
  [ "$DRFP" = "$FP_BEFORE" ] && ok "stock git cloned the BUCKET over dumb HTTP — identical content" \
    || bad "DR clone content differs: $DRFP"
  (cd "$TMP/dr" && git fsck --strict >/dev/null 2>&1) && ok "DR clone passes fsck --strict" || bad "DR clone fsck failed"
else
  bad "dumb-HTTP clone of the bucket failed — the bucket is not a bare repository"
fi
kill "$(cat "$TMP/srv.pid" 2>/dev/null)" 2>/dev/null; pkill -f 'http.server 8899' 2>/dev/null

# And the offline recipe, for a runbook: two mkdir/cp steps turn the
# synced prefix into a repository stock git can open with no server.
mkdir -p "$TMP/bare.git/refs" && cp "$TMP/bare.git/info/refs" "$TMP/bare.git/packed-refs"
if git clone -q "$TMP/bare.git" "$TMP/offline" 2>/dev/null; then
  OFP=$(cd "$TMP/offline" && git ls-tree -r HEAD | sha256sum | cut -d' ' -f1)
  [ "$OFP" = "$FP_BEFORE" ] && ok "offline recovery (mkdir refs + cp info/refs packed-refs) — identical" \
    || bad "offline recovery content differs"
else
  bad "offline recovery recipe did not produce a clonable repository"
fi
rm -rf "$TMP"
echo ""; echo "══ $pass passed, $fail failed ══"
