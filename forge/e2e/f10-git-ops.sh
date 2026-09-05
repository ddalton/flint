#!/usr/bin/env bash
# EVERYDAY GIT, not just the falsifiers.
#
# The falsifiers test the edges. This tests the operations an agent
# actually performs in a day: branch, push, fetch, pull, merge, delete,
# tag, shallow clone, prune, force-push. Each leg says whether it
# expects git to SUCCEED or the POLICY to REFUSE — a refusal that the
# policy is supposed to produce is a PASS, and conflating the two is
# how a drill reports a working system as broken (or the reverse).
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
POD=${POD:-agent1}
pass=0; fail=0
ok()   { pass=$((pass+1)); printf '  PASS  %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf '  FAIL  %s\n' "$1"; }

run() { kubectl exec -n "$NS" "$POD" -- sh -c "$1" 2>&1; }

# One helper inside the pod: auth header + repo URL.
PRE='T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%s" "$T" | base64 -w0)"; G() { git -c http.extraHeader="$A" "$@"; }; U='"$DOOR"'/git/'"$NS"'/'"$REPO"'.git'

check() { # $1=label $2=expect(ok|refuse) $3=script
  local out; out=$(run "$PRE; $3"); local rc=$?
  case "$2" in
    ok)     if [ $rc -eq 0 ]; then ok "$1"; else bad "$1 -> $(echo "$out" | tail -2 | tr '\n' ' ')"; fi ;;
    refuse) if [ $rc -ne 0 ]; then ok "$1 (refused, as the policy says)"
            else bad "$1 -> SUCCEEDED but the policy should have refused it"; fi ;;
  esac
  printf '%s\n' "$out" | sed 's/^/        /' | tail -3
}

echo "══ everyday git against flint forge ══"

check "clone" ok '
  rm -rf /tmp/g && G clone -q $U /tmp/g && cd /tmp/g && test -f README.md'

check "branch + push to agent/*" ok '
  cd /tmp/g && git config user.email a@b.c && git config user.name ops
  git checkout -q -b agent/feat-x
  echo feature > feat.txt && git add -A && git commit -qm feat
  G push -q origin agent/feat-x'

check "fetch sees the new branch from a second clone" ok '
  rm -rf /tmp/g2 && G clone -q $U /tmp/g2 && cd /tmp/g2
  G fetch -q origin && git rev-parse --verify origin/agent/feat-x >/dev/null'

check "pull --ff-only after the other clone advances it" ok '
  cd /tmp/g && echo more >> feat.txt && git commit -qam more && G push -q origin agent/feat-x
  cd /tmp/g2 && git checkout -q -B agent/feat-x origin/agent/feat-x
  G pull -q --ff-only origin agent/feat-x && grep -q more feat.txt'

check "second branch + local merge + push the merge" ok '
  cd /tmp/g && git checkout -q -b agent/feat-y main 2>/dev/null || git checkout -q -b agent/feat-y
  echo why > why.txt && git add -A && git commit -qm why
  G push -q origin agent/feat-y
  git checkout -q agent/feat-x && git merge -q --no-ff -m "merge y into x" agent/feat-y
  G push -q origin agent/feat-x && test -f why.txt && test -f feat.txt'

check "delete a remote branch" ok '
  cd /tmp/g && G push -q origin :refs/heads/agent/feat-y
  ! G ls-remote --exit-code --heads origin agent/feat-y >/dev/null 2>&1'

check "fetch --prune reflects the deletion" ok '
  cd /tmp/g2 && G fetch -q --prune origin
  ! git rev-parse --verify -q origin/agent/feat-y >/dev/null'

check "shallow clone --depth 1" ok '
  rm -rf /tmp/g3 && G clone -q --depth 1 --branch agent/feat-x $U /tmp/g3
  cd /tmp/g3 && test -f feat.txt && test "$(git rev-list --count HEAD)" = "1"'

check "annotated tag push" refuse '
  cd /tmp/g && git tag -a v0.1 -m v0.1 && G push origin v0.1'

check "force-push a non-fast-forward to agent/*" refuse '
  cd /tmp/g && git checkout -q agent/feat-x && git reset -q --hard HEAD~2
  echo rewritten > feat.txt && git add -A && git commit -qm rewrite
  G push --force origin agent/feat-x'

check "direct push to protected main" refuse '
  cd /tmp/g2 && git checkout -q -B main origin/main && echo x >> README.md
  git config user.email a@b.c; git config user.name ops
  git commit -qam "touch main" && G push origin main'

check "refs/for/main merges cleanly" ok '
  cd /tmp/g2 && G push origin HEAD:refs/for/main'

check "pull on main sees the merged result" ok '
  cd /tmp/g2 && G fetch -q origin && git rev-parse origin/main >/dev/null
  test "$(git rev-parse origin/main)" = "$(git rev-parse HEAD)"'

# THE CONFLICT LEG, and two ways it can be faked. Content must be
# UNIQUE per run: a fixed "LEFT"/"RIGHT" pair silently becomes "already
# contained" once a previous run has left that content on main, and the
# branch that is supposed to conflict then has nothing to commit at all
# — the leg reports a refusal and proves nothing. Both happened here.
# The second branch must also come from the RECORDED base, not from
# `origin/main`, which a successful push has already advanced.
check "a conflicting merge request names the path and moves no ref" refuse '
  rm -rf /tmp/cf && G clone -q $U /tmp/cf && cd /tmp/cf
  git config user.email a@b.c; git config user.name conflict
  BASE=$(git rev-parse origin/main); S=$(date +%s%N)
  git checkout -q -b l$S $BASE  && printf "LEFT-%s\n"  "$S" > README.md && git add -A && git commit -qm left
  git checkout -q -b r$S $BASE && printf "RIGHT-%s\n" "$S" > README.md && git add -A && git commit -qm right
  G push -q origin l$S:refs/for/main
  G fetch -q origin; AFTER=$(git rev-parse origin/main)
  G push origin r$S:refs/for/main; rc=$?
  G fetch -q origin
  test "$(git rev-parse origin/main)" = "$AFTER" || { echo "MAIN MOVED ON A CONFLICT"; exit 1; }
  exit $rc'


echo ""
echo "══ $pass passed, $fail failed ══"
