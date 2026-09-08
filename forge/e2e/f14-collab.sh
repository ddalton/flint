#!/usr/bin/env bash
# F14 — N agents editing ONE file, through one door.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/f14-collab.sh
#
# STATUS: WRITTEN, NEVER RUN ON A CLUSTER. Every number and every
# verdict below is a claim about what the drill WILL measure, not a
# result. Treat the first run as a run of the drill, not of forge.
#
# THE QUESTION. Several agents are given the same file and told to edit
# it. Git gives them no lock and forge gives them no queue they can see:
# each pulls, edits, commits and pushes, and the loser of a race is told
# `non-fast-forward` and must go round again. Does that loop converge —
# every agent's work landing, none of them starving — or does it wedge?
#
# F2 already proved the ARBITRATION: two clients push to one ref and
# exactly one is told ok. This is the other half, and it is a question
# about the CLIENTS: given a correct refusal, does the retry loop an
# agent harness would write actually terminate, and what does an agent
# have to look at to resolve the conflict it finds?
#
# NOTHING HERE CAN DEADLOCK, and saying so precisely matters. There is
# no mutual exclusion between clients — no client holds anything another
# client waits on — so the failure mode is not deadlock but STARVATION:
# a retry loop that keeps losing to fresher writers and never lands.
# That is what P3 measures, and it is measured as a distribution of
# attempts, never as a wall-clock number.
#
# HOW A GREEN RUN COULD MEAN NOTHING. Seven ways, each with its leg:
#
#  1. THE AGENTS NEVER CONTENDED. If they happen to serialise, every
#     push is a fast-forward, nothing is ever refused, and "no
#     starvation" is true of a drill that raced nothing. P1 counts the
#     refusals and is INCONCLUSIVE — never PASS — at zero.
#  2. "EVERYONE LANDED" WHILE NOBODY LANDED. A loop that exits on its
#     first error and reports success passes on total failure. Every
#     leg gates on the FINAL FILE CONTENT, by multiset, not on exit
#     codes.
#  3. A REBASE QUIETLY LOSES A LINE. Presence is not enough: a merge
#     that drops one agent and duplicates another still has "all the
#     names". P1 compares the exact multiset of N x E lines.
#  4. THE ARBITRATION ISN'T REAL. If a stale push were accepted, the
#     convergence in P1 would prove nothing at all. P4 pushes from a
#     deliberately stale clone with NO retry and requires a refusal.
#  5. THE CONFLICT ARM NEVER CONFLICTED. In P2 every agent rewrites the
#     SAME line, so a rebase must report a conflict. P2 requires at
#     least one, or it is inconclusive.
#  6. THE RESOLUTION POLICY IS UNTESTED. P5 is arm A with exactly ONE
#     dimension changed — the resolution flag, `-X ours`, which is what
#     an agent harness reaches for first and which silently discards the
#     agent's own edit during a rebase. That leg is EXPECTED to lose
#     work; it FAILS if it does not, because a run that lost nothing did
#     not reproduce the hazard it names.
#  7. THE COMPARISON IS MISSING. Arms A-C are the loop an agent harness
#     writes when nobody tells it otherwise. Forge has a mechanism that
#     removes the loop — refs/for/<target>, where the SERVER merges
#     inside the batch and never checks the client's old oid — so P7
#     runs the same work through it, in two sub-legs that must not be
#     confused: D uses content that cannot collide and must land on the
#     first attempt with zero refusals; E uses the same contested file
#     and is EXPECTED to be refused, because merge-tree is the same
#     three-way merge the client would run, only at a fresher base.
#
# THE ARMS. Same repository, same pods, same loop, same bounds; each
# labels the lines it writes, so no arm can be credited with another's
# work when they all edit one file in sequence.
#
#   A   append   + union    the baseline: disjoint edits, careful merge
#   B   headline + theirs   one contested line, last writer wins
#   C   append   + ours     A with ONE flag changed — the data-loss control
#   D   refs/for, own file  the server merges; no rebase, no retry, no refusal
#   E   refs/for, one file  the server merges, and still disagrees
set -uo pipefail
NS=${NS:-agents}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
TAG=${TAG:?set TAG to the image tag this drill deployed}
: "${BUCKET:?}"; : "${PREFIX:?}"
REPO=${REPO:-f14}
AGENTS=${AGENTS:-4}          # concurrent editors
EDITS=${EDITS:-5}            # edits each, in arm A
MAX_ATTEMPTS=${MAX_ATTEMPTS:-40}   # per edit; the starvation bound
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK" || { echo "cannot create WORK=$WORK"; exit 2; }
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

# Every agent's git identity, credential helper and remote, in one
# place so the arms cannot drift apart.
SETUP="git config --global credential.helper '!f(){ echo username=x; echo password=\$(cat /var/run/secrets/forge/token); };f' &&
       git config --global user.email a@b.c &&
       git config --global init.defaultBranch agents &&
       git config --global pull.rebase true &&
       git config --global rebase.autoStash false"

agent_name() { printf 'f14-a%s' "$1"; }

echo "== P0: the repository, the agents, and a push that works at all =="
sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" forge/e2e/f14-repo.yaml.tpl | K apply -f - >/dev/null

for i in $(seq 1 "$AGENTS"); do
  A=$(agent_name "$i")
  AGENT=$A TAG=$TAG envsubst '$AGENT $TAG' < forge/e2e/agent.yaml.tpl | K apply -f - >/dev/null
done
PODS=$(for i in $(seq 1 "$AGENTS"); do printf 'pod/%s ' "$(agent_name "$i")"; done)
# shellcheck disable=SC2086
K wait -n "$NS" --for=condition=Ready $PODS --timeout=300s >/dev/null 2>&1

for _ in $(seq 1 60); do
  ph=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$ph" = "Ready" ] && break
  sleep 5
done
[ "${ph:-}" = "Ready" ] && ok "the repository is Ready" \
  || { bad "the repository never became Ready (phase=${ph:-none}); the rest measures nothing"; exit 1; }

# Seed the shared file from ONE agent. Everything below builds on this
# commit, so if this fails nothing after it is interpretable.
A1=$(agent_name 1)
K exec -n "$NS" "$A1" -- sh -c "$SETUP &&
  rm -rf /tmp/seed && mkdir -p /tmp/seed && cd /tmp/seed && git init -q &&
  printf 'HEADLINE: none yet\n' > shared.md &&
  git add shared.md && git -c user.name=seed commit -qm seed &&
  git push -q $DOOR/git/$NS/$REPO.git agents:agents" >/dev/null 2>&1 \
  && ok "the shared file exists and one agent can push through the door" \
  || { bad "the seed push failed; F14 cannot proceed"; exit 1; }

# ── the collaboration loop, as an agent harness would write it ───────
#
# Deliberately naive in the ways that matter, and careful in the one
# that does not: it retries on refusal (that is the behaviour under
# test) and it never force-pushes (that would make every arm pass by
# destroying the other agents' work).
#
# It reports, per edit: attempts, and the reason for each failure. The
# COUNTS are the oracle; the wall-clock is only printed.
loop_body() { # <agent-index> <label> <content: append|headline> <resolution: union|theirs|ours>
  local i=$1 label=$2 content=$3 res=$4
  cat <<LOOP
$SETUP &&
git config --global user.name agent$i &&
rm -rf /tmp/w && git clone -q $DOOR/git/$NS/$REPO.git /tmp/w && cd /tmp/w &&
: > /tmp/report &&
for e in \$(seq 1 $EDITS); do
  # ONE commit per edit, made BEFORE the retry loop. The loop then only
  # ever replays it — which is what a real harness does, and what keeps
  # a loop that re-commits per attempt from stacking a commit for every
  # race it lost.
  case "$content" in
    append) printf '$label agent%s line %s\n' "$i" "\$e" >> shared.md ;;
    *)      sed -i "1s|.*|HEADLINE: $label agent$i edit \$e|" shared.md ;;
  esac
  git add shared.md && git commit -qm "$label agent$i edit \$e" >/dev/null 2>&1 || true
  n=0
  while : ; do
    n=\$((n+1))
    if [ \$n -gt $MAX_ATTEMPTS ]; then echo "edit=\$e STARVED attempts=\$n" >> /tmp/report; break; fi
    if git push -q origin agents 2>/tmp/perr; then
      echo "edit=\$e LANDED attempts=\$n" >> /tmp/report; break
    fi
    if ! grep -Eqi 'non-fast-forward|fetch first|rejected' /tmp/perr; then
      # Not a lost race. Retrying will not fix it, and counting it as
      # contention would inflate every number this drill prints.
      echo "edit=\$e ERROR attempts=\$n \$(tr -d '\n' < /tmp/perr | cut -c1-160)" >> /tmp/report
      break
    fi
    echo "edit=\$e REFUSED attempts=\$n" >> /tmp/report
    # LOST THE RACE. Take the new tip and replay this commit onto it.
    if [ "$res" = "ours" ]; then
      # THE NAIVE POLICY, and the reason arm C exists. During a rebase
      # "ours" is the UPSTREAM side being replayed onto, so -X ours
      # discards the INCOMING edit — this agent's own — and the rebase
      # then succeeds. The agent is told nothing.
      git pull -q --rebase -X ours origin agents >/dev/null 2>&1 || {
        echo "edit=\$e REBASE-FAILED attempts=\$n" >> /tmp/report
        git rebase --abort >/dev/null 2>&1 || true; }
      continue
    fi
    git pull -q --rebase origin agents >/dev/null 2>&1 || {
      echo "edit=\$e CONFLICT attempts=\$n" >> /tmp/report
      for f in \$(git diff --name-only --diff-filter=U); do
        if [ "$res" = "theirs" ]; then
          # ONE line, two values: there is no union. --theirs during a
          # rebase is the commit being REPLAYED, i.e. this agent's, so
          # this is last-writer-wins and P2 asks which writer that was.
          git checkout --theirs -- "\$f" >/dev/null 2>&1 || true
        fi
        # UNION. Both agents appended their own line at the end of the
        # file, so both belong in the result and the conflict is
        # positional rather than semantic. Dropping the three marker
        # lines keeps BOTH halves, which is what lets P1 then require
        # every edit exactly once.
        sed -i -e '/^<<<<<<< /d' -e '/^=======\$/d' -e '/^>>>>>>> /d' "\$f"
        git add "\$f"
      done
      GIT_EDITOR=true git rebase --continue >/dev/null 2>&1 || {
        git rebase --abort >/dev/null 2>&1 || true; }; }
  done
done
LOOP
}

run_arm() { # <label> <content> <resolution>
  local label=$1 content=$2 res=$3 i A
  note "arm $label: $content edits, '$res' resolution ($AGENTS agents x $EDITS edits, bound $MAX_ATTEMPTS)"
  for i in $(seq 1 "$AGENTS"); do
    A=$(agent_name "$i")
    K exec -n "$NS" "$A" -- sh -c "$(loop_body "$i" "$label" "$content" "$res")" \
      > "$WORK/$label-run-$i.log" 2>&1 &
  done
  wait
  for i in $(seq 1 "$AGENTS"); do
    A=$(agent_name "$i")
    K exec -n "$NS" "$A" -- cat /tmp/report > "$WORK/$label-report-$i.txt" 2>/dev/null
  done
  cat "$WORK/$label"-report-*.txt > "$WORK/$label-all.txt" 2>/dev/null
}

# How many of this arm's OWN lines are in the file, each exactly once.
# Every arm labels its lines, so a later arm cannot be credited with an
# earlier arm's work.
present_once() { # <label> <file> -> count
  local label=$1 file=$2 i e c n=0
  for i in $(seq 1 "$AGENTS"); do
    for e in $(seq 1 "$EDITS"); do
      c=$(grep -c "^$label agent$i line $e\$" "$file" 2>/dev/null || echo 0)
      [ "$c" -eq 1 ] && n=$((n+1))
    done
  done
  printf '%s' "$n"
}

final_file() { # -> stdout
  K exec -n "$NS" "$A1" -- sh -c "$SETUP &&
    rm -rf /tmp/f && git clone -q $DOOR/git/$NS/$REPO.git /tmp/f &&
    cat /tmp/f/shared.md" 2>/dev/null
}

echo
echo "== P1: arm A — disjoint appends, resolved as a UNION =="
run_arm A append union
final_file > "$WORK/A-final.txt"

refused=$(grep -c 'REFUSED' "$WORK/A-all.txt" 2>/dev/null || echo 0)
landed=$(grep -c 'LANDED'  "$WORK/A-all.txt" 2>/dev/null || echo 0)
starved=$(grep -c 'STARVED' "$WORK/A-all.txt" 2>/dev/null || echo 0)
want=$((AGENTS * EDITS))

# TRAP 1. Zero refusals means the agents never raced, and every claim
# below would be about a serial run wearing a concurrent run's name.
if [ "$refused" -eq 0 ]; then
  inconc "no push was ever refused — the agents did not contend, so convergence proves nothing"
else
  ok "the agents genuinely contended ($refused pushes refused)"
fi

# TRAPS 2 and 3. Not "did they finish" but "is the file right", counted
# by MULTISET: a merge that drops one agent's line and duplicates
# another's still contains every agent's name.
here=$(present_once A "$WORK/A-final.txt")
if [ "$here" -eq "$want" ]; then
  ok "every one of the $want edits is in the final file exactly once"
else
  bad "only $here of $want edits are present exactly once in the final file"
fi
[ "$landed" -eq "$want" ] && ok "every edit reported LANDED ($landed/$want)" \
  || bad "only $landed of $want edits landed"
[ "$starved" -eq 0 ] && ok "no agent hit the $MAX_ATTEMPTS-attempt bound" \
  || bad "$starved edits starved at the bound — the retry loop does not converge under this load"

echo "== P3: the shape of the contention, per agent =="
# A DISTRIBUTION, not a mean. One agent that lost every race is the
# finding; an average would hide it.
for i in $(seq 1 "$AGENTS"); do
  f="$WORK/A-report-$i.txt"
  [ -s "$f" ] || { note "agent$i produced no report"; continue; }
  tot=$(awk -F'attempts=' '{s+=$2} END{print s+0}' "$f")
  max=$(awk -F'attempts=' '{if($2+0>m)m=$2+0} END{print m+0}' "$f")
  note "agent$i: $(grep -c LANDED "$f") landed, $(grep -c REFUSED "$f") refusals, attempts total=$tot max=$max"
done
maxall=$(awk -F'attempts=' '{if($2+0>m)m=$2+0} END{print m+0}' "$WORK/A-all.txt")
note "worst single edit took $maxall attempts (bound $MAX_ATTEMPTS)"

echo
echo "== P4: the control — a stale push, with no retry, must be refused =="
# Without this, P1's convergence is compatible with forge accepting
# everything. Two clones of the same tip; one pushes, then the other
# pushes its own commit built on the now-stale tip.
K exec -n "$NS" "$A1" -- sh -c "$SETUP && git config --global user.name stale &&
  rm -rf /tmp/s1 /tmp/s2 &&
  git clone -q $DOOR/git/$NS/$REPO.git /tmp/s1 &&
  git clone -q $DOOR/git/$NS/$REPO.git /tmp/s2 &&
  cd /tmp/s1 && echo winner >> shared.md && git commit -aqm winner && git push -q origin agents &&
  cd /tmp/s2 && echo loser  >> shared.md && git commit -aqm loser  && git push -q origin agents" \
  > "$WORK/P4.log" 2>&1
rc=$?
if [ $rc -ne 0 ] && grep -qi 'non-fast-forward\|fetch first\|rejected' "$WORK/P4.log"; then
  ok "a stale push is refused, so P1's convergence is arbitration and not permissiveness"
elif [ $rc -ne 0 ]; then
  bad "the stale push failed for some OTHER reason: $(head -3 "$WORK/P4.log" | tr '\n' ' ')"
else
  bad "a stale push was ACCEPTED — every convergence result above is void"
fi
# Leave the branch clean for arm B.
K exec -n "$NS" "$A1" -- sh -c "$SETUP && cd /tmp/s1 && git pull -q --rebase origin agents" >/dev/null 2>&1

echo
echo "== P2: arm B — every agent rewrites the SAME line =="
run_arm B headline theirs
final_file > "$WORK/B-final.txt"
conflicts=$(grep -c 'CONFLICT' "$WORK/B-all.txt" 2>/dev/null || echo 0)
blanded=$(grep -c 'LANDED' "$WORK/B-all.txt" 2>/dev/null || echo 0)
bstarved=$(grep -c 'STARVED' "$WORK/B-all.txt" 2>/dev/null || echo 0)

# TRAP 5. If nothing conflicted, this arm is arm A with a different
# string and says nothing about conflict resolution.
if [ "$conflicts" -eq 0 ]; then
  inconc "no rebase conflicted — arm B did not exercise the case it exists for"
else
  ok "the same-line edits genuinely conflicted ($conflicts rebase conflicts)"
fi
[ "$blanded" -eq "$want" ] && ok "every same-line edit eventually landed ($blanded/$want)" \
  || bad "only $blanded of $want same-line edits landed"
[ "$bstarved" -eq 0 ] && ok "no agent starved under same-line contention" \
  || bad "$bstarved same-line edits starved at the $MAX_ATTEMPTS-attempt bound"

# WHO WON is a finding, not a pass. On one line there is no merge — the
# last writer's value is the value — and that is what an agent harness
# has to be told.
head1=$(head -1 "$WORK/B-final.txt")
note "the surviving headline is: $head1"
if printf '%s' "$head1" | grep -q '^HEADLINE: B agent'; then
  ok "the surviving line is one agent's whole value, not a merge of several"
else
  bad "the surviving line is neither an agent's value nor the seed: $head1"
fi
# …and arm A was not collateral damage.
survivedA=$(present_once A "$WORK/B-final.txt")
[ "$survivedA" -eq "$want" ] && ok "all $want of arm A's lines survived arm B untouched" \
  || bad "arm B destroyed arm A's content: only $survivedA of $want lines left"

echo "== P5: the naive resolution, as a positive control on data loss =="
# `-X ours` is the first thing an agent harness reaches for when a
# rebase conflicts, and it is WRONG: during a rebase "ours" is the
# upstream side being replayed onto, so the flag silently discards the
# INCOMING edit — the agent's own — and the rebase then succeeds. The
# agent is told it landed.
#
# Arm C is arm A with ONE dimension changed: the resolution flag. Same
# content shape, same agents, same bound. So a difference between them
# is attributable, and there is nowhere else for it to have come from.
#
# This leg is EXPECTED to lose work. It FAILS if it does not, because a
# run where nothing was lost did not reproduce the hazard it names.
run_arm C append ours
final_file > "$WORK/C-final.txt"
clanded=$(grep -c 'LANDED' "$WORK/C-all.txt" 2>/dev/null || echo 0)
crefused=$(grep -c 'REFUSED' "$WORK/C-all.txt" 2>/dev/null || echo 0)
chere=$(present_once C "$WORK/C-final.txt")

note "arm C: $clanded/$want reported LANDED, $crefused refusals, $chere/$want lines actually in the file"
note "arm A: $landed/$want reported LANDED, $refused refusals, $(present_once A "$WORK/C-final.txt")/$want lines still in the file"
if [ "$crefused" -eq 0 ]; then
  inconc "arm C never lost a race, so it never reached the resolution step it is testing"
elif [ "$clanded" -eq "$want" ] && [ "$chere" -lt "$want" ]; then
  ok "'-X ours' reported all $want edits landed while only $chere reached the file — the hazard reproduces, silently"
elif [ "$chere" -eq "$want" ]; then
  bad "'-X ours' lost nothing here — this control is not reproducing the hazard it names"
else
  bad "arm C is in neither state: landed=$clanded here=$chere want=$want"
fi

echo "== P6: what an agent can actually look at to resolve =="
# The user's hypothesis is that an agent reads git history. This checks
# that the history it would read is THERE and is legible through the
# door — a rebased branch could have flattened it.
hist=$(K exec -n "$NS" "$A1" -- sh -c "$SETUP &&
  rm -rf /tmp/h && git clone -q $DOOR/git/$NS/$REPO.git /tmp/h && cd /tmp/h &&
  git log --oneline | wc -l" 2>/dev/null | tr -d ' \r')
if [ "${hist:-0}" -ge "$want" ]; then
  ok "the branch carries $hist commits — an agent can read who changed what, and when"
else
  bad "only ${hist:-0} commits are reachable; at least $want edits were pushed"
fi
blame=$(K exec -n "$NS" "$A1" -- sh -c "cd /tmp/h && git log -1 --format=%an -- shared.md" 2>/dev/null | tr -d ' \r')
[ -n "$blame" ] && ok "the last writer of the file is attributable: $blame" \
  || bad "the file's last writer is not attributable through the door"

echo
echo "== P7: refs/for — the server merges, and what that does and does not buy =="
# THE MECHANISM. `judge_merge` (forge/syncer/src/batch.rs) does NOT
# compare the client's old oid against the target. It reads the target's
# CURRENT tip at judge time, inside the batch loop that is the
# repository's one writer, and merges the agent's commit onto that with
# `git merge-tree` in the bare repo. So a stale client is never told
# `fetch first`: there is nothing to retry, and therefore nothing to
# starve.
#
# TWO SUB-LEGS, because the mechanism answers ONE of the two questions
# this drill asks and the honest thing is to separate them.
#
#   D — content that cannot collide. Isolates "does refs/for remove the
#       retry loop?" from "can a three-way merge merge this content?".
#       Every push must land on its FIRST attempt, with no refusal.
#   E — the same shared-file appends arms A-C used. Expected to
#       CONFLICT, because merge-tree is the same three-way merge the
#       client would have run, only at a fresher base. That is the
#       finding, not a failure: refs/for removes the RACE, it does not
#       remove the DISAGREEMENT.

echo "-- D: disjoint content (each agent owns its own file) --"
for i in $(seq 1 "$AGENTS"); do
  A=$(agent_name "$i")
  K exec -n "$NS" "$A" -- sh -c "$SETUP && git config --global user.name agent$i &&
    rm -rf /tmp/d && git clone -q $DOOR/git/$NS/$REPO.git /tmp/d && cd /tmp/d && : > /tmp/dreport &&
    for e in \$(seq 1 $EDITS); do
      printf 'D agent%s line %s\n' '$i' \"\$e\" >> agent$i.md
      git add agent$i.md && git commit -qm 'D agent$i edit \$e' >/dev/null 2>&1
      # ONE attempt. No pull, no rebase, no retry — the claim is that
      # none of those are needed.
      if git push -q origin HEAD:refs/for/agents 2>/tmp/derr; then
        echo \"edit=\$e LANDED attempts=1\" >> /tmp/dreport
      else
        echo \"edit=\$e REFUSED attempts=1 \$(tr -d '\n' < /tmp/derr | cut -c1-200)\" >> /tmp/dreport
      fi
    done" > "$WORK/D-run-$i.log" 2>&1 &
done
wait
for i in $(seq 1 "$AGENTS"); do
  A=$(agent_name "$i")
  K exec -n "$NS" "$A" -- cat /tmp/dreport > "$WORK/D-report-$i.txt" 2>/dev/null
done
cat "$WORK"/D-report-*.txt > "$WORK/D-all.txt" 2>/dev/null

dlanded=$(grep -c 'LANDED'  "$WORK/D-all.txt" 2>/dev/null || echo 0)
drefused=$(grep -c 'REFUSED' "$WORK/D-all.txt" 2>/dev/null || echo 0)
[ "$dlanded" -eq "$want" ] \
  && ok "all $want disjoint edits landed on the FIRST attempt — no pull, no rebase, no retry" \
  || bad "only $dlanded of $want disjoint refs/for edits landed first time ($drefused refused)"
[ "$drefused" -eq 0 ] || {
  bad "a disjoint refs/for push was refused at all — refs/for is doing staleness arbitration:"
  grep 'REFUSED' "$WORK/D-all.txt" | head -3 | sed 's/^/        /'
}
# The content, per agent's own file.
dmissing=0
for i in $(seq 1 "$AGENTS"); do
  got=$(K exec -n "$NS" "$A1" -- sh -c "$SETUP &&
    rm -rf /tmp/dv && git clone -q $DOOR/git/$NS/$REPO.git /tmp/dv &&
    grep -c '^D agent$i line ' /tmp/dv/agent$i.md" 2>/dev/null | tr -d ' \r')
  [ "${got:-0}" -eq "$EDITS" ] || { dmissing=$((dmissing+1)); note "agent$i has ${got:-0} of $EDITS lines"; }
done
[ "$dmissing" -eq 0 ] && ok "every agent's $EDITS lines are in its own file" \
  || bad "$dmissing agents lost content through refs/for"
# THE COMPARISON, which is the point of the sub-leg.
note "arm A (rebase-retry): $refused refusals, worst edit $maxall attempts"
note "arm D (refs/for):     $drefused refusals, 1 attempt per edit by construction"
mrg=$(K exec -n "$NS" "$A1" -- sh -c "cd /tmp/dv && git log --oneline --merges | wc -l" 2>/dev/null | tr -d ' \r')
note "the branch carries ${mrg:-0} merge commits, built by the SYNCER on the agents' behalf"

echo "-- E: the same contested file, through refs/for --"
for i in $(seq 1 "$AGENTS"); do
  A=$(agent_name "$i")
  K exec -n "$NS" "$A" -- sh -c "$SETUP && git config --global user.name agent$i &&
    rm -rf /tmp/e && git clone -q $DOOR/git/$NS/$REPO.git /tmp/e && cd /tmp/e && : > /tmp/ereport &&
    for e in \$(seq 1 $EDITS); do
      printf 'E agent%s line %s\n' '$i' \"\$e\" >> shared.md
      git add shared.md && git commit -qm 'E agent$i edit \$e' >/dev/null 2>&1
      if git push -q origin HEAD:refs/for/agents 2>/tmp/eerr; then
        echo \"edit=\$e LANDED attempts=1\" >> /tmp/ereport
      else
        echo \"edit=\$e REFUSED attempts=1 \$(tr -d '\n' < /tmp/eerr | cut -c1-200)\" >> /tmp/ereport
      fi
    done" > "$WORK/E-run-$i.log" 2>&1 &
done
wait
for i in $(seq 1 "$AGENTS"); do
  A=$(agent_name "$i")
  K exec -n "$NS" "$A" -- cat /tmp/ereport > "$WORK/E-report-$i.txt" 2>/dev/null
done
cat "$WORK"/E-report-*.txt > "$WORK/E-all.txt" 2>/dev/null
final_file > "$WORK/E-final.txt"

elanded=$(grep -c 'LANDED'  "$WORK/E-all.txt" 2>/dev/null || echo 0)
erefused=$(grep -c 'REFUSED' "$WORK/E-all.txt" 2>/dev/null || echo 0)
econflict=$(grep -ci 'conflict' "$WORK/E-all.txt" 2>/dev/null || echo 0)
ehere=$(present_once E "$WORK/E-final.txt")
note "arm E: $elanded/$want landed, $erefused refused, of which $econflict say 'conflict', $ehere lines in the file"

# THE PROPERTY THAT MATTERS, and it is not "everything landed". Every
# refusal must be a CONTENT conflict: deterministic, a function of the
# two commits and not of who else was pushing. A refusal of any other
# kind would mean refs/for is arbitrating staleness after all, which
# would put the livelock back.
if [ "$erefused" -eq 0 ]; then
  ok "no refs/for push was refused at all"
elif [ "$erefused" -eq "$econflict" ]; then
  ok "every refs/for refusal is a content conflict — deterministic, so retrying unchanged cannot help, and cannot starve"
else
  bad "$erefused refs/for refusals of which only $econflict are content conflicts:"
  grep 'REFUSED' "$WORK/E-all.txt" | head -5 | sed 's/^/        /'
fi
# And the refusal must NAME the file, or an agent cannot act on it.
if [ "$econflict" -eq 0 ] || grep -qi 'conflict:.*shared.md' "$WORK/E-all.txt"; then
  [ "$econflict" -eq 0 ] || ok "the refusal names the conflicting path, so the agent knows where to look"
else
  bad "a conflict refusal did not name the path: $(grep -i conflict "$WORK/E-all.txt" | head -1)"
fi
# Nothing partial reached the branch: a refused merge must leave no
# trace, or the next agent inherits conflict markers.
if grep -q '^<<<<<<< \|^>>>>>>> ' "$WORK/E-final.txt"; then
  bad "a refused server merge left CONFLICT MARKERS in the published file"
else
  ok "no refused merge left conflict markers in the file"
fi
note "FINDING: refs/for removes the RACE (arm D: $drefused refusals in $want pushes)."
note "         It does not remove the DISAGREEMENT (arm E: $erefused refusals in $want)."
note "         merge-tree is the same three-way merge the client would run, at a fresher base."

echo
echo "F14: $PASS passed, $FAIL failed, $INCONC inconclusive"
echo "reports in $WORK"
[ "$FAIL" -eq 0 ] || exit 1
