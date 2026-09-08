#!/usr/bin/env bash
# M7 — the idle rung's durability. Push, let the repository be reaped for
# inactivity (replicas 0, the pod destroyed, its emptyDir cache with it),
# wake a FRESH process with a git request, and ask whether everything
# ACKNOWLEDGED before the reap is still there — by content, not by ref.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/m7-run.sh
#
# WHAT MAKES THIS DRILL NON-VACUOUS. Four ways a green "the data is all
# there" means nothing, each with the leg that closes it:
#
#  1. The reap never happened. Then nothing was ever at risk and the
#     clone reads a server that never died. P2 PROVES it: replicas
#     observed at 0 AND the pod's UID changes. "It looks restarted" is
#     not proof, and a reap that does not happen is a FAIL, never a skip.
#  2. The successor is not actually fresh. P3 reads `foldsCommitted` back
#     to 0 — fold counters are PROCESS memory, so a zero there and refs
#     that survive is the pair that means "new process, old bucket".
#  3. The clone is not really a clone. P4 clones into an empty directory
#     with no alternates and no local objects, so nothing can be served
#     out of a cache the drill itself warmed.
#  4. The check cannot fail. P5 is the negative control: a ref the branch
#     policy REFUSED and a commit that was never pushed must both be
#     ABSENT. Without it "everything I asked for is present" passes on a
#     server that simply says yes.
set -uo pipefail
NS=${NS:-agents}; AGENT=${AGENT:-agent1}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?}"; : "${PREFIX:?}"
REPO=${REPO:-m7}
# PAST THE 64-PACK CAP, deliberately. At 40 the repository never folds,
# so `foldsCommitted` was 0 BEFORE the reap as well as after, and P3's
# freshness check passed by being constant — it could not have failed.
# A cap-forced fold makes the counter non-zero going in, which is what
# turns it into a discriminator.
N=${N:-70}                     # commits pushed before the reap
REAP_WAIT=${REAP_WAIT:-420}    # how long to wait for replicas 0
WAKE_WAIT=${WAKE_WAIT:-240}
RUN=${RUN:-$(date +%s)}
WORK=${WORK:-$(mktemp -d)}
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }
leg()    { printf '\n== %s ==\n' "$*"; }
inpod()  { K -n "$NS" exec "$AGENT" -c agent -- sh -c "$*" 2>&1; }
put_script() { K -n "$NS" exec -i "$AGENT" -c agent -- sh -c "cat > /work/$1 && chmod +x /work/$1"; }
armenv() { echo "DOOR=$DOOR NS=$NS"; }

repo_pod()  { K -n "$NS" get pods -l "chert.us/repo=$REPO" -o json 2>/dev/null \
  | jq -r '[.items[]|select(.metadata.deletionTimestamp==null)]|sort_by(.metadata.creationTimestamp)|last|.metadata.name // empty'; }
repo_uid()  { K -n "$NS" get pods -l "chert.us/repo=$REPO" -o json 2>/dev/null \
  | jq -r '[.items[]|select(.metadata.deletionTimestamp==null)]|sort_by(.metadata.creationTimestamp)|last|.metadata.uid // empty'; }
replicas()  { K -n "$NS" get deploy "forge-$REPO" -o jsonpath='{.spec.replicas}' 2>/dev/null; }
status_port() { K -n "$NS" get deploy "forge-$REPO" -o jsonpath='{.spec.template.spec.containers[?(@.name=="syncer")].ports[?(@.name=="status")].containerPort}' 2>/dev/null; }
repo_status() { local p port; p=$(repo_pod); port=$(status_port)
  [ -n "$p" ] && [ -n "$port" ] && K -n "$NS" exec "$p" -c syncer -- wget -qO- "http://127.0.0.1:$port/status" 2>/dev/null; }
folds()  { repo_status | jq -r '.foldsCommitted // empty'; }
cp_alive() { K get --raw /readyz >/dev/null 2>&1 && echo yes || echo no; }

install_scripts() {
  put_script lib.sh <<'EOS'
auth() { T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf 'x:%s' "$T" | base64 -w0)"; }
G() { auth; git -c http.extraHeader="$A" "$@"; }
url() { echo "$DOOR/git/$NS/$1.git"; }
EOS
  # Content is DERIVED FROM THE COMMIT INDEX, so a server that returns
  # the right ref with the wrong bytes is caught. A ref SHA alone would
  # not catch it: the SHA is what the client sent, so comparing it to
  # itself proves only that a name was stored.
  put_script m7push.sh <<'EOS'
# m7push.sh <repo> <ref> <n> <tag>   -> prints "<i> <sha> <rc>" per commit
. /work/lib.sh
repo=$1; ref=$2; n=$3; tag=$4; d=/work/m7-$tag
rm -rf "$d"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email m7@invalid; git config user.name m7
i=1
while [ $i -le $n ]; do
  # deterministic, verifiable, and different for every commit
  printf 'm7-%s-commit-%d-payload\n' "$tag" "$i" > "file-$i.txt"
  echo "$i" > counter.txt
  git add -A >/dev/null; git commit -qm "m7 c$i" >/dev/null
  sha=$(git rev-parse HEAD)
  G push -q "$(url "$repo")" "HEAD:refs/heads/$ref" >/dev/null 2>&1; rc=$?
  echo "$i $sha $rc"
  i=$((i+1))
done
EOS
  # The negative control's two halves.
  put_script m7neg.sh <<'EOS'
# m7neg.sh <repo> <tag>  -> prints "refused <sha> <rc>" and "unpushed <sha>"
. /work/lib.sh
repo=$1; tag=$2; d=/work/m7neg-$tag
rm -rf "$d"; mkdir -p "$d"; cd "$d"
git init -q -b main; git config user.email m7@invalid; git config user.name m7
echo "refused-payload-$tag" > refused.txt; git add -A >/dev/null
git commit -qm "refused" >/dev/null; rsha=$(git rev-parse HEAD)
# refs/heads/nope matches neither `main` nor the agentPattern `agent/*`
G push -q "$(url "$repo")" "HEAD:refs/heads/nope" >/dev/null 2>&1; rc=$?
echo "refused $rsha $rc"
echo "never-pushed-payload-$tag" > unpushed.txt; git add -A >/dev/null
git commit -qm "unpushed" >/dev/null; echo "unpushed $(git rev-parse HEAD)"
EOS
  put_script m7verify.sh <<'EOS'
# m7verify.sh <repo> <ref> <tag> <n>  -> "clone rc", then "<i> <content>" per commit
. /work/lib.sh
repo=$1; ref=$2; tag=$3; n=$4; d=/work/m7v-$tag
# A FRESH directory, no alternates, no reference repo: nothing here can
# be served out of the objects the push leg left behind.
rm -rf "$d"
G clone -q --no-local --single-branch --branch "$ref" "$(url "$repo")" "$d" >/dev/null 2>&1
echo "clone $?"
[ -d "$d" ] || exit 0
cd "$d"
echo "head $(git rev-parse HEAD 2>/dev/null)"
i=1
while [ $i -le $n ]; do
  if [ -f "file-$i.txt" ]; then echo "$i $(cat "file-$i.txt")"; else echo "$i MISSING"; fi
  i=$((i+1))
done
EOS
}

# ── P0 ───────────────────────────────────────────────────────────────
leg_P0() {
  leg "P0 preconditions: the repository serves, the idle rung is CONFIGURED, and the drill can see both"
  [ "$(cp_alive)" = yes ] && ok "control plane answers /readyz" || { bad "control plane is not answering — every leg below is VOID"; return 1; }
  K -n "$NS" get pod "$AGENT" -o jsonpath='{.status.phase}' 2>/dev/null | grep -q Running \
    && ok "agent $AGENT is Running" || { bad "agent is not Running"; return 1; }
  install_scripts; ok "push scripts installed on the agent"

  local after pod port f
  after=$(K -n "$NS" get flintrepo "$REPO" -o jsonpath='{.spec.idle.suspendAfterSecs}' 2>/dev/null)
  # READ IT BACK. A drill that waits for a reap on a repository whose
  # ladder is off waits forever and then reports a timeout as if it were
  # a durability finding.
  [ -n "$after" ] && [ "$after" -gt 0 ] 2>/dev/null \
    && ok "the idle rung is configured: suspendAfterSecs=$after" \
    || { bad "$REPO has no idle.suspendAfterSecs — there is no reap to test"; return 1; }

  pod=$(repo_pod); port=$(status_port)
  [ -n "$pod" ] && ok "the repository has a pod: $pod" || { bad "no pod for $REPO"; return 1; }
  [ -n "$port" ] && ok "status port derived from the deployment: $port" || { bad "no status port"; return 1; }
  f=$(folds)
  [ -n "$f" ] && ok "/status answers (foldsCommitted=$f) — freshness can be proven after the wake" \
    || { bad "/status does not answer; P3 could not tell a fresh process from a survivor"; return 1; }
  inpod "export $(armenv); . /work/lib.sh; G ls-remote \"\$(url $REPO)\" >/dev/null 2>&1; echo rc=\$?" | grep -q rc=0 \
    && ok "$REPO answers ls-remote through the door" || { bad "$REPO does not answer ls-remote"; return 1; }
}

# ── P1 ───────────────────────────────────────────────────────────────
leg_P1() {
  leg "P1: $N commits pushed and ACKNOWLEDGED, with content derived from the commit index"
  inpod "export $(armenv); /work/m7push.sh $REPO agent/m7-$RUN $N m7-$RUN" > "$WORK/push.txt" 2>&1
  local acked
  acked=$(awk '$3==0' "$WORK/push.txt" | wc -l | tr -d ' ')
  note "acknowledged: $acked of $N"
  [ "${acked:-0}" -eq "$N" ] && ok "every push was acknowledged — all $N are now claimed durable" \
    || { bad "only $acked of $N acknowledged; the drill cannot claim what was never acked"; return 1; }
  awk '$3==0 {print $2}' "$WORK/push.txt" | tail -1 > "$WORK/head-sha"
  note "head at reap time: $(cat "$WORK/head-sha")"

  inpod "export $(armenv); /work/m7neg.sh $REPO m7-$RUN" > "$WORK/neg.txt" 2>&1
  local nrc
  nrc=$(awk '$1=="refused"{print $3}' "$WORK/neg.txt")
  # The negative control must itself be REFUSED, or P5 proves nothing.
  [ "${nrc:-0}" != "0" ] \
    && ok "the branch policy refused refs/heads/nope (rc=$nrc) — P5 has something to look for" \
    || { bad "refs/heads/nope was ACCEPTED — the negative control is void and P5 cannot fail"; }
  UID_BEFORE=$(repo_uid); FOLDS_BEFORE=$(folds)
  note "pod uid before the reap: $UID_BEFORE, foldsCommitted=$FOLDS_BEFORE"
  # The counter is only evidence of a lost process if it was NON-ZERO
  # first. Say so here, where it is still cheap to fix, rather than
  # letting P3 report a constant as a pass.
  [ "${FOLDS_BEFORE:-0}" -gt 0 ] 2>/dev/null \
    && ok "foldsCommitted=$FOLDS_BEFORE before the reap — a zero after it will MEAN something" \
    || inconc "foldsCommitted is 0 before the reap; P3's freshness check cannot discriminate and will say so"
}

# ── P2 ───────────────────────────────────────────────────────────────
leg_P2() {
  leg "P2: PROVE the reap — replicas observed at 0 and the pod gone"
  local t0 now r pod saw0=no
  t0=$(date +%s)
  while :; do
    now=$(date +%s); [ $((now - t0)) -ge "$REAP_WAIT" ] && break
    r=$(replicas); pod=$(repo_pod)
    if [ "${r:-1}" = "0" ]; then saw0=yes; fi
    if [ "$saw0" = yes ] && [ -z "$pod" ]; then
      ok "reaped after $((now - t0)) s: replicas=0 AND no pod — the process and its emptyDir cache are gone"
      REAP_SECS=$((now - t0)); return 0
    fi
    sleep 5
  done
  # A reap that does not happen is a FAILURE OF THE DRILL'S PREMISE, not
  # a durability result. Reporting the clone as green here would be the
  # vacuous pass this leg exists to prevent.
  bad "no reap within ${REAP_WAIT}s (replicas=$(replicas), pod=$(repo_pod)) — nothing was ever at risk, so P4 would prove NOTHING"
  return 1
}

# ── P3 ───────────────────────────────────────────────────────────────
leg_P3() {
  leg "P3: wake with a git request and prove the successor is a FRESH process"
  local t0 t1 rc uid_after f
  t0=$(date +%s)
  inpod "export $(armenv); . /work/lib.sh; G ls-remote \"\$(url $REPO)\" >/dev/null 2>&1; echo rc=\$?" > "$WORK/wake.txt" 2>&1
  t1=$(date +%s)
  rc=$(grep -o 'rc=[0-9]*' "$WORK/wake.txt" | tail -1 | cut -d= -f2)
  [ "${rc:-1}" = "0" ] && ok "the door woke it and held the request: ls-remote answered in $((t1-t0)) s" \
    || { bad "ls-remote failed after the reap (rc=$rc) — the wake path is broken"; return 1; }
  uid_after=$(repo_uid)
  [ -n "$uid_after" ] && [ "$uid_after" != "$UID_BEFORE" ] \
    && ok "the pod UID changed ($UID_BEFORE -> $uid_after) — this is a different process, not a survivor" \
    || { bad "pod UID unchanged ($uid_after) — the pod never died and every check below is vacuous"; return 1; }
  f=$(folds)
  # foldsCommitted is PROCESS memory. Zero here, with refs that survive,
  # is the pair that means "new process reading the old bucket" — but
  # ONLY if it was non-zero before. A counter that read 0 on both sides
  # is a constant, and a constant cannot be evidence of a change.
  if [ "${FOLDS_BEFORE:-0}" -le 0 ] 2>/dev/null; then
    inconc "foldsCommitted was 0 before the reap and is $f after — CONSTANT, so this check discriminates nothing (the UID change above is what carries P3)"
  elif [ "${f:-x}" = "0" ]; then
    ok "foldsCommitted fell $FOLDS_BEFORE -> 0 across the reap — process memory was lost, so the bucket is what answers"
  else
    bad "foldsCommitted=$f on the successor after $FOLDS_BEFORE before — process memory SURVIVED; this is not a fresh process"
  fi
}

# ── P4 ───────────────────────────────────────────────────────────────
leg_P4() {
  leg "P4: clone into an EMPTY directory and verify CONTENT, not ref existence"
  inpod "export $(armenv); /work/m7verify.sh $REPO agent/m7-$RUN m7-$RUN $N" > "$WORK/verify.txt" 2>&1
  local crc head want got missing=0 wrong=0 i
  crc=$(awk '$1=="clone"{print $2}' "$WORK/verify.txt")
  [ "${crc:-1}" = "0" ] && ok "the fresh clone succeeded against the woken server" \
    || { bad "clone failed (rc=$crc) after the reap"; return 1; }
  head=$(awk '$1=="head"{print $2}' "$WORK/verify.txt")
  [ "$head" = "$(cat "$WORK/head-sha")" ] \
    && ok "HEAD matches the last acknowledged push: $head" \
    || bad "HEAD is $head but the last ACK was $(cat "$WORK/head-sha") — an acknowledged commit did not survive"
  i=1
  while [ $i -le "$N" ]; do
    want="m7-m7-$RUN-commit-$i-payload"
    got=$(awk -v k="$i" '$1==k {sub(/^[0-9]+ /,""); print}' "$WORK/verify.txt")
    if [ "$got" = "MISSING" ] || [ -z "$got" ]; then missing=$((missing+1))
    elif [ "$got" != "$want" ]; then wrong=$((wrong+1)); fi
    i=$((i+1))
  done
  [ "$missing" -eq 0 ] && [ "$wrong" -eq 0 ] \
    && ok "all $N commits' CONTENT verified byte-for-byte after the reap" \
    || bad "$missing missing and $wrong wrong of $N after the reap — acknowledged data did not survive"
}

# ── P5 ───────────────────────────────────────────────────────────────
leg_P5() {
  leg "P5 NEGATIVE CONTROL: what was never durable must be ABSENT"
  local rsha usha out
  rsha=$(awk '$1=="refused"{print $2}' "$WORK/neg.txt")
  usha=$(awk '$1=="unpushed"{print $2}' "$WORK/neg.txt")
  out=$(inpod "export $(armenv); . /work/lib.sh; G ls-remote \"\$(url $REPO)\" 2>/dev/null")
  echo "$out" | grep -q "refs/heads/nope" \
    && bad "refs/heads/nope is PRESENT after the wake — the branch policy did not hold across the reap" \
    || ok "the policy-refused ref is absent after the wake"
  # And the commit that was never pushed must not resolve. If it does,
  # the "clone" was reading something other than this server.
  echo "$out" | grep -q "$usha" \
    && bad "a commit that was NEVER PUSHED resolves on the server ($usha)" \
    || ok "the never-pushed commit does not resolve — the clone really is the server's data"
  [ -n "$rsha" ] && [ -n "$usha" ] || inconc "the negative control's SHAs were not captured; P5 scored nothing"
}

main() {
  echo "M7 — repo=$REPO bucket=$BUCKET prefix=$PREFIX commits=$N run=$RUN work=$WORK"
  leg_P0 || { echo; echo "P0 failed — refusing to run the measurement legs."; exit 1; }
  if [ "$FAIL" -ne 0 ]; then echo; echo "P0 recorded $FAIL failure(s) — refusing to continue."; exit 1; fi
  leg_P1 || { echo; echo "P1 failed — nothing was acknowledged, so there is nothing to prove durable."; exit 1; }
  leg_P2 || { echo; echo "P2 failed — the reap did not happen; a green P4 here would be vacuous."; exit 1; }
  leg_P3
  leg_P4
  leg_P5
  echo
  echo "== RESULT =="
  [ "$(cp_alive)" = yes ] || echo "  *** CONTROL PLANE GONE — THIS RUN IS VOID, NOT GREEN ***"
  echo "  reap observed after ${REAP_SECS:-?} s; $N commits pushed and acknowledged"
  echo "  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC   (INCONCLUSIVE is not PASS)"
}
main "$@"
