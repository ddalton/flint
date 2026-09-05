#!/usr/bin/env bash
# FALSIFIER 9 — the legible export.
#
# Two claims, and they pull in opposite directions:
#
#  (a) LEGIBILITY. Every exported file is byte-identical to
#      `git show main:<path>`, readable straight out of the bucket by
#      something that knows nothing about git or forge.
#  (b) O(CHANGED). A push touching three files rewrites three objects,
#      not the whole tree. Legibility is worthless if publishing it
#      costs a full rewrite each time.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }
gitq(){ kubectl exec -n "$NS" deploy/forge-proj -c syncer -- git --git-dir=/repo/$NS/$REPO.git "$@" 2>/dev/null; }
snap(){ aws s3 cp "s3://$BUCKET/$PREFIX/git/git/snapshot" - 2>/dev/null; }
# KEY FIRST, then the timestamp, and a PLAIN sort. `comm` compares
# whole lines in lexicographic order, so `sort -k2` — which orders by
# one field while comm compares the whole line — makes it emit garbage:
# the first version of this reported 59 changed objects of which 58 
# carried the OLD timestamp.
listing(){ aws s3 ls "s3://$BUCKET/$PREFIX/export/files/" --recursive | awk '{print $4" "$1"T"$2}' | sort; }
# What the SERVER believes it has exported. The snapshot's
# `exported_commit` lags by design — a bundle or export never spends a
# CAS of its own, it is stashed and the NEXT batch carries it — so the
# last push of a drill can never see it there. Ask the pod instead.
export_record(){ kubectl exec -n "$NS" deploy/forge-proj -c syncer -- \
  sh -c 'cat /repo/'"$NS"'/'"$REPO"'.git/flint-forge/export/tree.record.json 2>/dev/null' \
  | jq -r '.commit // "none"' 2>/dev/null; }

echo "══ F9: the legible export ══"
MAIN=$(gitq rev-parse refs/heads/main)
EXPORTED=$(export_record)
echo "main=$MAIN  export record=$EXPORTED  (snapshot: $(snap | jq -r '.exported_commit'))"
[ "$MAIN" = "$EXPORTED" ] && ok "the export has published main" || bad "export lags main ($EXPORTED vs $MAIN)"

echo "── (a) is every exported file byte-identical to git show? ──"
TMP=$(mktemp -d)
aws s3 sync "s3://$BUCKET/$PREFIX/export/files/" "$TMP/exp/" --quiet
COUNT=0; DIFF=0
while read -r path; do
  [ -z "$path" ] && continue
  COUNT=$((COUNT+1))
  A=$(gitq show "$MAIN:$path" | sha256sum | cut -d' ' -f1)
  B=$(sha256sum < "$TMP/exp/$path" 2>/dev/null | cut -d' ' -f1)
  [ "$A" = "$B" ] || { DIFF=$((DIFF+1)); echo "      DIFFERS: $path"; }
done <<EOF
$(gitq ls-tree -r --name-only "$MAIN")
EOF
[ "$COUNT" -gt 0 ] && [ "$DIFF" = 0 ] && ok "all $COUNT files byte-identical to git show main:<path>" \
  || bad "$DIFF of $COUNT files differ (or nothing was compared)"

# The converse: nothing in the export that is NOT in the tree.
EXTRA=$(comm -13 <(gitq ls-tree -r --name-only "$MAIN" | sort) <(cd "$TMP/exp" && find . -type f | sed 's|^\./||' | sort) | wc -l | tr -d ' ')
[ "$EXTRA" = 0 ] && ok "the export holds nothing the tree does not" || bad "$EXTRA stale file(s) left in the export"

echo "── (b) a three-file push must rewrite about three objects ──"
BEFORE=$(listing)
kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  G(){ git -c http.$DOOR/.extraHeader=\"\$H\" \"\$@\"; }
  rm -rf /tmp/ex && G clone -q $DOOR/git/$NS/$REPO.git /tmp/ex && cd /tmp/ex
  git config user.email e@x.y; git config user.name exporter
  S=\$(date +%s)
  for f in data/blob-0.bin data/blob-1.bin data/blob-2.bin; do echo \"changed \$S\" > \$f; done
  git add -A && git commit -qm 'three files' >/dev/null
  G push -q origin HEAD:refs/for/main" >/dev/null 2>&1
NEWMAIN=$(kubectl exec -n "$NS" agent1 -- sh -c 'cd /tmp/ex && git rev-parse HEAD' 2>/dev/null)
echo "   pushed $NEWMAIN"

for i in $(seq 1 24); do
  [ "$(export_record)" = "$NEWMAIN" ] && { echo "   export caught up after ~$((i*5))s"; break; }
  sleep 5
done
[ "$(export_record)" = "$NEWMAIN" ] && ok "the export followed the push" || bad "the export never caught up"

AFTER=$(listing)
CHANGED=$(comm -13 <(echo "$BEFORE") <(echo "$AFTER") | wc -l | tr -d ' ')
TOTAL=$(echo "$AFTER" | wc -l | tr -d ' ')
echo "   $CHANGED of $TOTAL exported objects were rewritten"
# ZERO IS NOT A PASS. If the export never advanced, nothing was
# rewritten and `0 <= 6` scores as a triumph — which is exactly what
# the first run of this drill reported while the export was in fact
# frozen with every file parked.
if [ "$CHANGED" = 0 ]; then
  bad "0 objects rewritten — the export did not advance, so O(changed) measured nothing"
elif [ "$CHANGED" -le 6 ]; then
  ok "O(changed): $CHANGED objects rewritten for a 3-file push (of $TOTAL)"
else
  bad "$CHANGED objects rewritten for a 3-file change — that is not O(changed)"
fi

# And the three files must now read back as the new content.
BAD3=0
for f in data/blob-0.bin data/blob-1.bin data/blob-2.bin; do
  A=$(gitq show "$NEWMAIN:$f" | sha256sum | cut -d' ' -f1)
  B=$(aws s3 cp "s3://$BUCKET/$PREFIX/export/files/$f" - 2>/dev/null | sha256sum | cut -d' ' -f1)
  [ "$A" = "$B" ] || { BAD3=$((BAD3+1)); echo "      $f differs after the update"; }
done
[ "$BAD3" = 0 ] && ok "the three changed files read back identical from the bucket" || bad "$BAD3 changed file(s) wrong in the export"
rm -rf "$TMP"
echo ""; echo "══ $pass passed, $fail failed ══"
