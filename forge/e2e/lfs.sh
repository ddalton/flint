#!/usr/bin/env bash
# GIT LFS — the multi-modal case.
#
# Forge's reason for carrying LFS is that agents working on images,
# audio and video should not be pushing those bytes through the git
# object database. The batch API lives in the SYNCER, not the door: the
# door deliberately holds no bucket credential, and a presigned URL can
# only be minted by something that has one.
#
# What this checks, in order of how quietly each can fail:
#   1. the batch endpoint answers at all, with the right media type
#   2. an upload actually places the object in the bucket
#   3. a FRESH clone gets the real bytes back, not the pointer
#   4. the pointer in git is a pointer, not the payload
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }

echo "══ LFS ══"
echo "── does the client image even have git-lfs? ──"
HAS=$(kubectl exec -n "$NS" agent1 -- sh -c 'command -v git-lfs >/dev/null && echo yes || echo no' 2>/dev/null)
echo "   git-lfs present in the agent image: $HAS"

echo "── (1) the batch endpoint ──"
OUT=$(kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token)
  wget -q -O- --header=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\" \
    --header='Content-Type: application/vnd.git-lfs+json' \
    --header='Accept: application/vnd.git-lfs+json' \
    --post-data='{\"operation\":\"upload\",\"transfers\":[\"basic\"],\"objects\":[{\"oid\":\"$(printf abc | sha256sum | cut -d' ' -f1)\",\"size\":3}]}' \
    $DOOR/git/$NS/$REPO.git/info/lfs/objects/batch 2>&1" 2>&1)
echo "$OUT" | head -c 600 | sed 's/^/      /'
echo ""
case "$OUT" in
  *'"actions"'*|*'"objects"'*) ok "the batch endpoint answered with an LFS document" ;;
  *) bad "no LFS batch response" ;;
esac

echo "── (2)(3)(4) a real upload and a fresh clone ──"
if [ "$HAS" = yes ]; then
  R=$(kubectl exec -n "$NS" agent1 -- sh -c "
    T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
    G(){ git -c http.$DOOR/.extraHeader=\"\$H\" \"\$@\"; }
    rm -rf /tmp/lfs && G clone -q $DOOR/git/$NS/$REPO.git /tmp/lfs && cd /tmp/lfs
    git config user.email l@x.y; git config user.name lfs
    git lfs install --local >/dev/null 2>&1
    git lfs track '*.bin2' >/dev/null 2>&1
    dd if=/dev/urandom of=big.bin2 bs=1M count=4 status=none
    sha256sum big.bin2 | cut -d' ' -f1 > /tmp/lfs.sha
    git add .gitattributes big.bin2 && git commit -qm lfs
    G push origin HEAD:refs/heads/agent/lfs 2>&1 | tail -2
    echo PTR:\$(git cat-file -p HEAD:big.bin2 | head -1)")
  echo "$R" | sed 's/^/      /'
  case "$R" in *"version https://git-lfs"*) ok "git stores a POINTER, not the payload" ;; *) bad "no LFS pointer in the tree" ;; esac
  case "$R" in *"agent/lfs"*|*"->"*) ok "the LFS push was accepted" ;; *) bad "the push did not land" ;; esac

  V=$(kubectl exec -n "$NS" agent1 -- sh -c "
    T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
    rm -rf /tmp/lfs2
    git -c http.$DOOR/.extraHeader=\"\$H\" clone -q --branch agent/lfs $DOOR/git/$NS/$REPO.git /tmp/lfs2 2>&1 | tail -1
    cd /tmp/lfs2 && sha256sum big.bin2 2>/dev/null | cut -d' ' -f1")
  WANT=$(kubectl exec -n "$NS" agent1 -- cat /tmp/lfs.sha 2>/dev/null)
  GOT=$(echo "$V" | tail -1)
  [ -n "$WANT" ] && [ "$WANT" = "$GOT" ] && ok "a fresh clone recovered the 4 MiB payload byte-identically" \
    || bad "clone did not recover the payload (want ${WANT:0:16}… got ${GOT:0:16}…)"

  N=$(aws s3 ls "s3://$BUCKET/$PREFIX/git/lfs/" --recursive 2>/dev/null | wc -l | tr -d ' ')
  [ "${N:-0}" -gt 0 ] && ok "the payload is in the bucket under lfs/ ($N object(s))" \
    || bad "nothing under the lfs/ prefix — the pointer has no backing object"
else
  echo "   SKIPPED (2)(3)(4): the agent image has no git-lfs client."
  echo "   This is a REAL gap for the multi-modal use case: forge serves the"
  echo "   batch API, but an agent image without git-lfs cannot use it."
fi
echo ""; echo "══ $pass passed, $fail failed ══"
