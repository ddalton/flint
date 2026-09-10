#!/usr/bin/env bash
# F17 — the idle ladder across TWO park cycles (X24).
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/f17-ladder.sh
#
# THE CLAIM UNDER TEST. A repository that parks, is woken through the
# door, and goes quiet again PARKS AND STAYS PARKED.
#
# WHY THIS LEG EXISTS. Nothing clears `chert.us/requested-at` from a
# FlintRepo — `reconcile` says so ("deliberately NOT cleared: it is the
# door's heartbeat") — while `idle::decide`'s Suspended branch asks
# whether the KEY IS PRESENT. One pass after any real suspend the
# repository is Suspended carrying a stamp older than its own
# threshold, because a stamp older than the threshold is exactly what
# let it suspend. Presence reads that as a standing request: replicas
# go back to 1, the emptyDir is rebuilt, a full restore is paid, and
# the cycle repeats once per threshold forever.
#
# EVERY EXISTING LEG STOPS BEFORE THIS. F13's P6 and F12's P6 park a
# repository ONCE, and both do it on a repository the door has never
# woken — so no stamp exists and the park holds. F13 then DELETES the
# annotation by hand before its wake (`chert.us/requested-at-`), which
# is precisely the assist that hides this. The EC2 acceptance run of
# 2026-09-04 did the same thing by accident: one park, one wake, end of
# run.
#
# FIVE WAYS A GREEN RUN COULD BE VACUOUS, each with the leg that shuts
# it:
#
#  1. The probes don't work — a wrong label selector counts 0 pods
#     forever and every park "passes". L1 requires the SAME probes to
#     report 1 pod, replicas 1 and Active while the repository is up.
#  2. Park #1 proves nothing, because the repository had a stamp all
#     along. L2 asserts the annotation is ABSENT before the first park,
#     so park #1 and park #2 differ in exactly one thing: the stamp the
#     door wrote in between.
#  3. The door never armed anything, so wake #1 was some other event.
#     L3 requires the annotation to APPEAR without this script writing
#     it. Nothing here runs `kubectl annotate`.
#  4. The repository never really came down, so "it stayed down" is
#     trivially true. L4 requires replicas 0 AND zero pods, and L6
#     requires a CHANGED pod UID after each wake — a restore, not a
#     pod that never left.
#  5. It stayed down because the ladder is wedged, not because it is
#     correct. L6 wakes it a SECOND time and requires it to serve.
#
# L5 is the assertion this drill exists for: it SAMPLES the parked
# state for $HOLD seconds rather than reading it once. One flip to
# Active is the failure, and a single sample taken at the wrong moment
# would miss it.
set -uo pipefail
NS=${NS:-agents}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
TAG=${TAG:-dev}
: "${BUCKET:?set BUCKET}"; : "${PREFIX:?set PREFIX}"
REPO=${REPO:-f17}
# One agent pod PER REPOSITORY, and the name is not cosmetic:
# `spec.containers[].image` IS mutable on a running pod, so re-applying
# a shared agent under a different image tag restarts the container in
# place and takes `$HOME` — and the credential helper written into it —
# with it. That cost a control arm: the seed push passed in the old
# container and the wake clone failed in the new one, three legs later,
# looking exactly like a door that would not wake the repository.
AGENT=$REPO-agent
AFTER=${AFTER:-60}           # spec.idle.suspendAfterSecs
PARK_WAIT=${PARK_WAIT:-360}  # budget for a park to happen
WAKE_WAIT=${WAKE_WAIT:-300}  # budget for a clone through a parked repo
HOLD=${HOLD:-90}             # how long a park must HOLD, sampled
STEP=${STEP:-5}
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

sel="chert.us/repo=$REPO"
pods_n()   { K get -n "$NS" pod -l "$sel" --no-headers 2>/dev/null | grep -c . ; }
pod_uid()  { K get -n "$NS" pod -l "$sel" -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null; }
replicas() { K get -n "$NS" deploy -l "$sel" -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null; }
phase()    { K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null; }
istate()   { K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.metadata.annotations.chert\.us/idle-state}' 2>/dev/null; }
stamp()    { K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.metadata.annotations.chert\.us/requested-at}' 2>/dev/null; }
# The operator rewrites `idle-since` on EVERY ladder move. Comparing it
# across the hold catches a suspend/wake pair that happened entirely
# between two samples — which is exactly the shape of this defect, since
# the flip is one reconcile wide and a sampler can step straight over it.
isince()   { K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.metadata.annotations.chert\.us/idle-since}' 2>/dev/null; }

# A park is replicas 0 AND no pod. Either alone can lie: replicas 0
# with a pod still terminating is not down yet, and zero pods with
# replicas 1 is a scheduling failure wearing a park's clothes.
wait_park() { # <budget-secs>
  local budget=$1 t=0
  while [ "$t" -lt "$budget" ]; do
    [ "$(replicas)" = "0" ] && [ "$(pods_n)" = "0" ] && return 0
    sleep "$STEP"; t=$((t+STEP))
  done
  return 1
}

wait_phase() { # <phase> <budget>
  local want=$1 budget=$2 t=0
  while [ "$t" -lt "$budget" ]; do
    [ "$(phase)" = "$want" ] && return 0
    sleep "$STEP"; t=$((t+STEP))
  done
  return 1
}

# A clone through the door. This is the ONLY thing in this script that
# ever asks for the repository back — no annotation is written here.
CRED='!f(){ echo username=x; echo password=$(cat /var/run/secrets/forge/token); };f'
clone() { # <dir>
  # `-c credential.helper` on the command itself, not `git config
  # --global`: a config file is state in a container, and a container
  # can be restarted under you.
  K exec -n "$NS" "$AGENT" -- sh -c "
    rm -rf $1 && git -c credential.helper='$CRED' clone -q $DOOR/git/$NS/$REPO.git $1" \
    >/dev/null 2>&1
}

echo "== L0: a repository with the ladder armed, and one push to date =="
sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" -e "s|__AFTER__|$AFTER|g" \
    forge/e2e/f17-repo.yaml.tpl | K apply -f - >/dev/null
K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: $AGENT, namespace: $NS, labels: { role: forge-agent } }
spec:
  serviceAccountName: agent-runner
  restartPolicy: Never
  containers:
    - name: c
      image: dilipdalton/flint-forge-git:$TAG
      command: ["sleep","36000"]
      volumeMounts: [{ name: t, mountPath: /var/run/secrets/forge, readOnly: true }]
  volumes:
    - name: t
      projected:
        sources: [{ serviceAccountToken: { path: token, audience: forge.chert.us, expirationSeconds: 3600 } }]
EOF
K wait -n "$NS" --for=condition=Ready pod/$AGENT --timeout=180s >/dev/null 2>&1 \
  || { bad "the agent pod never became Ready — nothing else can run"; exit 1; }
wait_phase Ready 300 || { bad "the repository never reached Ready"; exit 1; }
K exec -n "$NS" "$AGENT" -- sh -c "
  git config --global user.email a@b.c && git config --global user.name agent &&
  rm -rf /tmp/seed &&
  git -c credential.helper='$CRED' clone -q $DOOR/git/$NS/$REPO.git /tmp/seed && cd /tmp/seed &&
  { git checkout -q main 2>/dev/null || git checkout -q -b main; } &&
  echo f17-seed > seed.txt && git add -A && git commit -qm seed &&
  git -c credential.helper='$CRED' push -q origin HEAD:main" >/dev/null 2>&1 \
  && ok "seeded main through the door (a push, so the syncer's own clock starts here)" \
  || { bad "the seed push failed — the idle clock would be the pod's start time instead"; }

echo "== L1: the probes report a LIVE repository (the control arm) =="
# Without this, every "0 pods" below could be a broken selector.
n=$(pods_n); r=$(replicas); s=$(istate)
[ "$n" = "1" ] && ok "the pod probe counts the live pod: $n" \
  || bad "the pod probe says $n with the repository up — every park assertion below would be vacuous"
[ "$r" = "1" ] && ok "the replica probe reads 1" || bad "the replica probe reads '$r'"
# ABSENT IS ACTIVE, and that is the code's rule rather than a gap:
# `state_of` is `unwrap_or(IdleState::Active)` and the operator patches
# the annotation only when the ladder MOVES, so a repository that has
# never parked carries no annotation at all. Requiring the literal
# string here failed a correct system on the first run.
case "$s" in
  Active|"") ok "the ladder reads Active (${s:-absent, which IS Active})" ;;
  *)         bad "the ladder reads '$s' on a repository that has never parked" ;;
esac

echo "== L2: park #1, on a repository the door has NEVER woken =="
st=$(stamp)
[ -z "$st" ] && ok "no wake stamp exists yet — park #1 and park #2 differ in exactly that" \
  || inconc "a stamp already exists ($st); park #1 no longer differs from park #2 in one thing"
uid1=$(pod_uid)
if wait_park "$PARK_WAIT"; then
  ok "park #1: replicas 0, no pods (phase $(phase))"
else
  bad "the repository never parked within ${PARK_WAIT}s — nothing below measures anything"
  echo "  RESULT  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC"; exit 1
fi

echo "== L3: a clone wakes it, and THE DOOR arms the stamp =="
t0=$(date +%s)
if clone /tmp/w1; then
  t1=$(date +%s)
  ok "a git clone through a parked repository completed in $((t1-t0))s"
else
  bad "the clone through the parked repository failed"
fi
uid2=$(pod_uid)
[ -n "$uid2" ] && [ "$uid2" != "$uid1" ] \
  && ok "and it is a FRESH pod ($uid1 -> $uid2) — a real restore" \
  || bad "the pod UID did not change ($uid1 -> $uid2); this was not a park and a restore"
armed=$(stamp)
[ -n "$armed" ] && ok "the DOOR armed the wake stamp: $armed (nothing in this script writes it)" \
  || bad "no stamp appeared, so the wake came from something other than the door"

echo "== L4: it goes quiet again and parks a SECOND time =="
wait_phase Ready 300 || inconc "the repository did not report Ready after the wake"
if wait_park "$PARK_WAIT"; then
  ok "park #2: replicas 0, no pods (phase $(phase))"
else
  bad "it never parked a second time within ${PARK_WAIT}s — with the stamp now present this is
       the other face of the same defect: a repository that can never sleep again"
  echo "  RESULT  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC"; exit 1
fi

echo "== L5: and it STAYS parked (${HOLD}s, sampled AND clock-checked) =="
since0=$(isince)
note "idle-since at the start of the hold: ${since0:-<unset>}"
flip=""; t=0
while [ "$t" -lt "$HOLD" ]; do
  st8=$(istate); r=$(replicas); n=$(pods_n)
  if [ "$st8" != "Suspended" ] || [ "$r" != "0" ] || [ "$n" != "0" ]; then
    flip="at +${t}s: idle-state=$st8 replicas=$r pods=$n phase=$(phase) stamp=$(stamp)"
    break
  fi
  sleep "$STEP"; t=$((t+STEP))
done
# THE SAMPLING-INDEPENDENT ORACLE. A suspend/wake pair is one reconcile
# wide, so a ${STEP}s sampler can step over it and see Suspended on both
# sides. `idle-since` cannot be stepped over: every ladder move rewrites
# it, so an unchanged value is the only reading that means "nothing
# moved" rather than "nothing was seen to move".
since1=$(isince)
if [ -n "$since0" ] && [ "$since1" != "$since0" ]; then
  bad "THE LADDER MOVED DURING THE HOLD without a sample catching it: idle-since $since0 -> $since1"
elif [ -z "$since0" ]; then
  inconc "idle-since was never set, so the clock oracle could not run; only the samples count"
else
  ok "idle-since is unchanged across the hold ($since1) — no ladder move was missed between samples"
fi
if [ -z "$flip" ]; then
  ok "the park held for ${HOLD}s across $((HOLD/STEP)) samples"
else
  bad "THE PARK DID NOT HOLD — $flip"
  note "the operator's own account of the pass that undid it:"
  for p in $(K get pod -n "${NS_SYS:-forge-system}" -o name 2>/dev/null | grep -i operator); do
    K logs -n "${NS_SYS:-forge-system}" "$p" --tail=60 2>/dev/null | grep -i "$REPO" | tail -20
  done
fi

echo "== L6: a second wake, so L5 did not pass on a wedged ladder =="
# THE DOOR'S HALF OF X24, and the state that pins it: the repository is
# parked and CARRYING the stamp the door wrote at L3 — a stamp that is
# now older than the threshold, because being older than the threshold
# is what let it park at all. This is the exact state where the two
# halves must agree. If the door read presence, it would decline to
# re-arm (a stamp is there!) and an operator that requires a LIVE
# request would hold forever: the repository could never be woken
# again. So the wake below is not just "it came back" — the stamp's
# VALUE has to move, and that is the door re-arming.
parked_with=$(stamp)
[ -n "$parked_with" ] \
  && ok "it parked while CARRYING a stamp ($parked_with) — unlike park #1, which had none" \
  || inconc "no stamp is present, so this leg cannot test the door's half at all"
t0=$(date +%s)
if clone /tmp/w2; then
  t1=$(date +%s)
  ok "a second clone woke it again in $((t1-t0))s"
else
  bad "the second clone failed — the repository parked and could not be woken"
fi
uid4=$(pod_uid)
[ -n "$uid4" ] && [ "$uid4" != "$uid2" ] \
  && ok "on another fresh pod ($uid2 -> $uid4)" \
  || bad "the pod UID did not change on the second wake ($uid2 -> $uid4)"
K exec -n "$NS" "$AGENT" -- sh -c "grep -q f17-seed /tmp/w2/seed.txt" >/dev/null 2>&1 \
  && ok "and the content survived both round trips" \
  || bad "the seed content is not in the second clone"

rearmed=$(stamp)
if [ -z "$parked_with" ]; then
  inconc "no stamp was carried into the wake, so nothing can be said about the re-arm"
elif [ "$rearmed" = "$parked_with" ]; then
  bad "THE DOOR DID NOT RE-ARM: the stamp is still $rearmed. Against an operator that
       requires a live request this is the wedge — the repository woke here only because
       something else raised it"
else
  ok "the DOOR RE-ARMED a stale stamp: $parked_with -> $rearmed"
fi

echo "  RESULT  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC"
[ "$FAIL" = "0" ]
