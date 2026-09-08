#!/usr/bin/env bash
# F15 — a person at the door: an issuer-minted JWT as the principal.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... KEYDIR=... ./forge/e2e/f15-jwt.sh
#
# STATUS: WRITTEN, NEVER RUN. Phase 1 of the Knox/JWT design is covered
# by 151 unit tests and a 12/12 mutation matrix and has NEVER EXECUTED
# ON A CLUSTER. Everything below is a claim about what it will measure.
#
# WHAT ONLY A CLUSTER CAN SHOW. The unit tests prove the door's
# arithmetic: this signature verifies, that `aud` is refused, the router
# reaches this verifier. None of them can show the thing the design
# actually claims —
#
#   * that a git CLIENT can carry a JWT at all (the design says the door
#     already takes the Basic password as an opaque token, so no
#     protocol change is needed; that has never been tried);
#   * that a person's `sub` survives the door, the hook, `pre-receive`
#     and the syncer to become a COMMIT AUTHOR;
#   * that branch policy keyed on a person actually bounds that person —
#     the whole point of the feature, and the limitation the
#     architecture document called unfixable while every principal was a
#     ServiceAccount;
#   * that both credential kinds work through ONE door in one
#     deployment, which is what "beside TokenReview" means.
#
# THE TOKENS ARE MINTED ON THE OPERATOR'S MACHINE with `openssl`, from a
# keypair generated for this run; only the PUBLIC half is mounted in the
# cluster. That was verified locally against the door's own verifier
# before this drill was written, so a refusal here is the door's answer
# and not a malformed token.
#
# HOW A GREEN RUN COULD MEAN NOTHING. Six ways, each with its leg:
#
#  1. NOTHING WAS VERIFIED — the door is admitting anything. P1 sends an
#     unsigned token, one signed by a key the door does not hold, one
#     for another issuer and one for another audience, and requires all
#     four refused, with the good token accepted in the same leg.
#  2. THE PERSON WAS NEVER THE PRINCIPAL — the push worked because the
#     pod's own ServiceAccount was acceptable anyway. P2's repository
#     does NOT list `agent-runner`, so a pod token is refused there; the
#     JWT is the only way in.
#  3. THE AUTHOR IS THE CLIENT'S GIT CONFIG, not the verified identity.
#     P3 sets `user.name`/`user.email` to a DIFFERENT person and
#     requires the committer recorded by forge to be the token's `sub`.
#  4. THE POLICY DID NOT BIND. P4 has alice push `agent/alice/x` (must
#     succeed) and `agent/bob/x` (must be refused), so the pattern is
#     doing work rather than admitting everything.
#  5. THE LIFETIME CEILING IS ORNAMENTAL. P5 mints a VALID signature
#     with a 30-day lifetime — the shape of Knox's 120-day default —
#     and requires refusal, with a 10-minute token accepted as control.
#  6. THE OPERATOR GUARD IS NOT WIRED. P6 reads `status.conditions` for
#     a repository listing `*` and requires ConsumersSound=False naming
#     WildcardAdmitsPeople, with a control repository listing alice
#     explicitly that stays True.
set -uo pipefail
NS=${NS:-agents}
NS_SYS=${NS_SYS:-forge-system}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
TAG=${TAG:?set TAG to the image tag this drill deployed}
: "${BUCKET:?}"; : "${PREFIX:?}"; : "${KEYDIR:?set KEYDIR to the scratch dir holding the keypair}"
ISS=${ISS:-https://drill.local/issuer}
# MUST match the door's `--jwt-key-id`, which the chart defaults to
# `static`. A token naming any other `kid` is refused before its
# signature is even checked — which would look exactly like a bad key.
KID=${KID:-static}
AUD=${AUD:-forge.chert.us}
ALICE=${ALICE:-alice@example.com}
BOB=${BOB:-bob@example.com}
REPO=${REPO:-f15}
REPO_WILD=${REPO_WILD:-f15wild}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK" || { echo "cannot create WORK=$WORK"; exit 2; }
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

MINT="$KEYDIR/mint.sh"
[ -x "$MINT" ] || { echo "no minter at $MINT"; exit 2; }
now() { date +%s; }
# <sub> [iss] [aud] [lifetime-secs] [key]
tok() {
  local sub=$1 iss=${2:-$ISS} aud=${3:-$AUD} life=${4:-600} key=${5:-$KEYDIR/jwt.pem} t
  t=$(now); "$MINT" "$key" "$KID" "$iss" "$sub" "$aud" "$t" "$((t + life))"
}

AGENT=f15-agent
# TWO pods, because the git image carries git and no curl (busybox wget
# only), and the status-code legs need curl. The split is not a
# workaround: the CURL pod has no forge token projected into it at all,
# so every 200 it gets is proof that the credential is the TOKEN and not
# the pod — which is the whole difference between a person and a
# ServiceAccount.
CURL=f15-curl
c() { # <curl args…> -> http code on stdout
  K exec -n "$NS" "$CURL" -- curl -sS -o /dev/null -w '%{http_code}' "$@" 2>/dev/null
}

echo "== P0: the rig, and a door that is actually verifying an issuer =="
# Two repositories differing in ONE line: the wildcard. P6's control
# depends on that being the only difference, so it is a template slot
# rather than a second file that could drift.
apply_repo() { # <name> <extra-consumers-line-or-empty>
  sed -e "s|__REPO__|$1|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
      -e "s|__PREFIX__|$PREFIX|g" -e "s|__ALICE__|$ALICE|g" \
      -e "s|__EXTRA__|$2|g" forge/e2e/f15-repo.yaml.tpl | K apply -f - >/dev/null
}
apply_repo "$REPO" ""
apply_repo "$REPO_WILD" '    - "*"\n'
AGENT=$AGENT TAG=$TAG envsubst '$AGENT $TAG' < forge/e2e/agent.yaml.tpl | K apply -f - >/dev/null
K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: $CURL, namespace: $NS }
spec:
  restartPolicy: Never
  containers:
    - name: c
      image: curlimages/curl:8.10.1
      command: ["sleep","36000"]
EOF
K wait -n "$NS" --for=condition=Ready "pod/$AGENT" "pod/$CURL" --timeout=300s >/dev/null 2>&1

for _ in $(seq 1 60); do
  ph=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$ph" = "Ready" ] && break
  sleep 5
done
[ "${ph:-}" = "Ready" ] && ok "the repository is Ready" \
  || { bad "the repository never became Ready (phase=${ph:-none}); the rest measures nothing"; exit 1; }

# THE GATE. If the door was deployed without --jwt-issuer, every leg
# below fails for one uninteresting reason. A good token must be
# accepted before anything else is believed.
GATE=$(tok "$ALICE")
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$GATE")
case "$code" in
  200) ok "the door verifies $ISS and admits $ALICE" ;;
  401) bad "a good token was refused — the door was probably deployed without --jwt-issuer"
       K -n "$NS_SYS" logs -l app.kubernetes.io/name=flint-forge-door --tail=20 2>/dev/null | sed 's/^/        /'
       echo "F15 cannot proceed."; exit 1 ;;
  *)   bad "a good token answered $code"; echo "F15 cannot proceed."; exit 1 ;;
esac

echo
echo "== P1: what the door must refuse, with the good token as control =="
# Each arm changes ONE thing about a token that is otherwise identical
# to the one just accepted.
BADSIG="${GATE%.*}.$(printf 'not-a-signature' | openssl base64 -A | tr '+/' '-_' | tr -d '=')"
declare -a ARMS=(
  "an unsigned/forged signature|$BADSIG"
  "a key the door does not hold|$(tok "$ALICE" "$ISS" "$AUD" 600 "$KEYDIR/other.pem")"
  "another issuer|$(tok "$ALICE" https://elsewhere.example "$AUD")"
  "another audience|$(tok "$ALICE" "$ISS" some-other-service)"
  "an expired token|$(t=$(now); "$MINT" "$KEYDIR/jwt.pem" "$KID" "$ISS" "$ALICE" "$AUD" $((t-7200)) $((t-3600)))"
)
for arm in "${ARMS[@]}"; do
  what=${arm%%|*}; t=${arm#*|}
  code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$t")
  [ "$code" = "401" ] && ok "$what: 401" || bad "$what answered $code, want 401"
done
# THE CONTROL, repeated after the refusals: the door still admits the
# good token, so the five 401s are judgements and not an outage.
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$(tok "$ALICE")")
[ "$code" = "200" ] && ok "and the good token still works, so those were judgements" \
  || bad "the door stopped admitting the good token ($code) — the refusals above prove nothing"

echo
echo "== P2: the JWT is the ONLY way in — a pod token is refused here =="
# $REPO does not list `agent-runner`, so the pod's own credential — the
# one every other forge drill uses — must not open it. Without this,
# every push below could be succeeding as the ServiceAccount.
PODTOK=$(K exec -n "$NS" "$AGENT" -- cat /var/run/secrets/forge/token 2>/dev/null | tr -d '\r\n')
[ -n "$PODTOK" ] || bad "could not read the agent's projected token — P2 measures nothing without it"
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$PODTOK")
[ "$code" = "403" ] && ok "the pod's ServiceAccount token: 403 — it is not a consumer here" \
  || bad "a pod token answered $code at a repository that does not list it"

echo
echo "== P3: a real git push, and where the verified identity lands =="
# The credential helper emits the JWT as the Basic PASSWORD, which is the
# design's claim that no protocol change is needed.
#
# WHAT FORGE CAN AND CANNOT DECIDE, corrected on the wire. A commit a
# CLIENT makes is authored by that client's git config and arrives
# inside a pack; forge cannot rewrite it without invalidating the pack,
# and does not try. The verified identity governs AUTHORIZATION (P4) and
# authors the commits the SERVER itself creates — a `refs/for` merge,
# or a file-API write. So `user.name`/`user.email` are set to SOMEONE
# ELSE below and both halves are asserted: the client's commit keeps the
# client's name, and the server's merge carries the token's `sub`.
T=$(tok "$ALICE" "$ISS" "$AUD" 900)
HELPER="git config --global credential.helper '!f(){ echo username=x; echo password=$T; };f'"
K exec -n "$NS" "$AGENT" -- sh -c "
  $HELPER &&
  git config --global user.email impostor@example.com &&
  git config --global user.name  impostor &&
  git config --global init.defaultBranch agents &&
  rm -rf /tmp/w && mkdir -p /tmp/w && cd /tmp/w && git init -q &&
  echo hello > a.txt && git add a.txt && git commit -qm 'from alice' &&
  git push -q $DOOR/git/$NS/$REPO.git agents:agents" > "$WORK/p3.log" 2>&1 \
  && ok "a git client pushed with a JWT as the Basic password — no protocol change" \
  || bad "the JWT push failed: $(tail -2 "$WORK/p3.log" | tr '\n' ' ')"

# A CLIENT's commit keeps the client's identity. Stated as a PASS
# because it is the correct behaviour, not a shortfall: anything else
# would mean forge rewriting objects it received.
cl=$(K exec -n "$NS" "$AGENT" -- sh -c "
  $HELPER && rm -rf /tmp/r && git clone -q -b agents $DOOR/git/$NS/$REPO.git /tmp/r &&
  cd /tmp/r && git log -1 --format='%an'" 2>/dev/null | tr -d '\r')
[ "$cl" = "impostor" ] \
  && ok "a client's own commit keeps the client's author ($cl) — forge does not rewrite a received pack" \
  || bad "the client's commit is authored '$cl', want 'impostor'"

# THE SERVER'S OWN COMMIT. Two clones from one tip: the first advances
# `agents`, the second is then stale and proposes a merge through
# `refs/for`, which forge performs with `merge-tree` inside the batch
# and commits AS THE PRINCIPAL.
merge_author=$(K exec -n "$NS" "$AGENT" -- sh -c "
  $HELPER &&
  rm -rf /tmp/m1 /tmp/m2 &&
  git clone -q -b agents $DOOR/git/$NS/$REPO.git /tmp/m1 &&
  git clone -q -b agents $DOOR/git/$NS/$REPO.git /tmp/m2 &&
  cd /tmp/m1 && echo one > one.txt && git add one.txt && git commit -qm first && git push -q origin agents &&
  cd /tmp/m2 && echo two > two.txt && git add two.txt && git commit -qm 'second, diverging' &&
  git push -q origin HEAD:refs/for/agents 2>/dev/null &&
  git fetch -q origin agents 2>/dev/null; git log -1 --format='%an' FETCH_HEAD" 2>/dev/null | tr -d '\r')
note "the server-created merge is authored: ${merge_author:-<none>}"
if printf '%s' "$merge_author" | grep -q "$ALICE"; then
  ok "the merge forge itself built carries the TOKEN's subject, not the client's git config"
elif printf '%s' "$merge_author" | grep -qi impostor; then
  bad "forge authored its own merge as the client's git config — the verified identity was not used"
else
  bad "the merge author is neither: ${merge_author:-<none>}"
fi

echo "== P4: branch policy keyed on a PERSON =="
# `agent/alice/*` is alice's; `agent/bob/*` is not. This is the
# limitation the architecture document records as unfixable while every
# principal is a ServiceAccount many pods share.
push_branch() { # <token> <branch> -> http-ish verdict in $?
  K exec -n "$NS" "$AGENT" -- sh -c "
    git config --global credential.helper '!f(){ echo username=x; echo password=$1; };f' &&
    cd /tmp/w && git checkout -q -B tmp && echo $2 >> a.txt &&
    git commit -aqm '$2' &&
    git push -q $DOOR/git/$NS/$REPO.git tmp:refs/heads/$2" >"$WORK/p4-$(echo "$2" | tr / -).log" 2>&1
}
if push_branch "$T" "agent/alice/x"; then
  ok "alice may push agent/alice/x"
else
  bad "alice cannot push her own branch: $(tail -2 "$WORK/p4-agent-alice-x.log" | tr '\n' ' ')"
fi
if push_branch "$T" "agent/bob/x"; then
  bad "alice pushed agent/bob/x — the per-person pattern bounds nothing"
else
  ok "alice may NOT push agent/bob/x — the pattern binds to the person"
fi
# And bob, whom the repository does not list at all, gets nowhere.
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$(tok "$BOB")")
[ "$code" = "403" ] && ok "a validly-signed token for $BOB: 403 — signature is not authority" \
  || bad "an unlisted person answered $code"

echo
echo "== P5: the lifetime ceiling refuses a VALID signature =="
# The shape of Knox's shipped 120-day default. Everything about this
# token is correct except how long it lives.
LONG=$(t=$(now); "$MINT" "$KEYDIR/jwt.pem" "$KID" "$ISS" "$ALICE" "$AUD" "$t" $((t + 30*86400)))
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$LONG")
[ "$code" = "401" ] && ok "a 30-day token is refused though its signature is good" \
  || bad "a 30-day token answered $code — the ceiling is ornamental"
# The control, differing ONLY in lifetime.
code=$(c "$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack" -u "x:$(tok "$ALICE" "$ISS" "$AUD" 600)")
[ "$code" = "200" ] && ok "the same token minted for 10 minutes is accepted" \
  || bad "the short token answered $code — P5's arms differ in more than lifetime"

echo
echo "== P6: the operator's D6a guard, on the wire =="
cond() { K get -n "$NS" flintrepo "$1" -o jsonpath='{.status.conditions[?(@.type=="ConsumersSound")]}' 2>/dev/null; }
for _ in $(seq 1 24); do [ -n "$(cond "$REPO_WILD")" ] && break; sleep 5; done
w=$(cond "$REPO_WILD"); n=$(cond "$REPO")
note "wildcard repo: ${w:-<no condition>}"
note "named repo:    ${n:-<no condition>}"
if [ -z "$w" ]; then
  inconc "no ConsumersSound condition was written — the operator predates the guard"
else
  printf '%s' "$w" | grep -q '"status":"False"' \
    && ok "the wildcard repository is ConsumersSound=False" \
    || bad "a repository listing `*` beside a person is not reported: $w"
  printf '%s' "$w" | grep -q 'WildcardAdmitsPeople' \
    && ok "and the reason names the wildcard" || bad "the reason is not WildcardAdmitsPeople: $w"
  # THE CONTROL: the same operator, the same issuer, a repository that
  # names alice explicitly — it must stay sound, or the guard is simply
  # complaining about everything.
  printf '%s' "$n" | grep -q '"status":"True"' \
    && ok "the repository that names $ALICE explicitly stays True" \
    || bad "the control repository is also reported: $n"
fi

echo
echo "F15: $PASS passed, $FAIL failed, $INCONC inconclusive"
echo "logs in $WORK"
[ "$FAIL" -eq 0 ] || exit 1
