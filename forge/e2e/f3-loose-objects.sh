#!/usr/bin/env bash
# FALSIFIER 3 — loose objects never leak.
#
# A `refs/for/*` merge is the one path where the SERVER creates objects:
# `merge-tree` and `commit-tree` write a merge commit and its trees, and
# they are written LOOSE. The bucket only ever holds packs, so if step 2
# did not pack them before the CAS, the snapshot would name a commit
# that is in no pack — and the next cold restore would come back with a
# ref pointing at nothing.
#
# The kill is what makes this real: without it the loose objects are
# still on the pod's disk and everything looks fine.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }
gitq(){ kubectl exec -n "$NS" deploy/forge-proj -c syncer -- git --git-dir=/repo/$NS/$REPO.git "$@"; }

echo "══ F3: a server-created merge commit must reach the bucket in a pack ══"
BASE=$(gitq rev-parse refs/heads/main)
S=$(date +%s)

# Two branches off ONE base touching DIFFERENT files: a genuine merge,
# not a fast-forward, so the server must create a commit of its own.
OUT=$(kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  G(){ git -c http.$DOOR/.extraHeader=\"\$H\" \"\$@\"; }
  rm -rf /tmp/m3 && G clone -q $DOOR/git/$NS/$REPO.git /tmp/m3 && cd /tmp/m3
  git config user.email m@x.y; git config user.name merger
  git checkout -q -b la$S $BASE && echo la$S > left-$S.txt  && git add -A && git commit -qm left
  G push -q origin la$S:refs/for/main || echo PUSH1-FAILED
  git checkout -q -b ra$S $BASE && echo ra$S > right-$S.txt && git add -A && git commit -qm right
  G push origin ra$S:refs/for/main 2>&1 | tail -2
  G fetch -q origin
  echo \"MERGED=\$(git rev-parse origin/main)\"
  echo \"MINE=\$(git rev-parse ra$S)\"")
echo "$OUT" | sed 's/^/    /'
MERGED=$(echo "$OUT" | sed -n 's/^MERGED=//p'); MINE=$(echo "$OUT" | sed -n 's/^MINE=//p')

[ -n "$MERGED" ] && [ "$MERGED" != "$MINE" ] && [ "$MERGED" != "$BASE" ] \
  && ok "the server created a merge commit ($MERGED), not a fast-forward" \
  || bad "no server-created merge happened — this leg tests nothing (main=$MERGED mine=$MINE base=$BASE)"

PARENTS=$(gitq rev-list --parents -n1 "$MERGED" | wc -w | tr -d ' ')
[ "$PARENTS" = "3" ] && ok "it is a real merge commit (two parents)" || bad "expected 2 parents, got $((PARENTS-1))"

# The snapshot must already name it — acknowledged means durable.
SNAPMAIN=$(aws s3 cp "s3://$BUCKET/$PREFIX/git/git/snapshot" - 2>/dev/null | jq -r '.refs["refs/heads/main"]')
[ "$SNAPMAIN" = "$MERGED" ] && ok "the bucket snapshot already names the merge commit" \
  || bad "snapshot says $SNAPMAIN, client was told $MERGED"

OLD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].metadata.name}')
echo "── killing the pod: the loose objects die with the emptyDir ──"
kubectl delete pod -n "$NS" "$OLD" --wait=true >/dev/null
kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null

gitq fsck --strict >/dev/null 2>&1 && ok "fsck --strict clean after the restore" || bad "fsck FAILED — an object the refs name is missing"
[ "$(gitq rev-parse refs/heads/main)" = "$MERGED" ] && ok "main still points at the merge commit" || bad "main moved across the restore"
# And the decisive one: the commit is in a PACK, with nothing loose.
LOOSE=$(gitq count-objects -v | awk -F': ' '/^count/{print $2}')
[ "$LOOSE" = "0" ] && ok "zero loose objects — everything came back from packs" || bad "$LOOSE loose objects after a restore"
gitq cat-file -e "$MERGED^{commit}" 2>/dev/null && ok "the merge commit is readable from the restored packs" || bad "the merge commit is not in the repository"
echo ""; echo "══ $pass passed, $fail failed ══"
