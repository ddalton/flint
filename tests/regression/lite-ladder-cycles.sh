#!/usr/bin/env bash
# The lite idle ladder, across a stamp that OUTLIVES its request.
#
#   KUBECONFIG=... ./tests/regression/lite-ladder-cycles.sh
#
# THE CLAIM UNDER TEST. A share that some client stamped while it was
# RUNNING, and then stopped asking about, parks and STAYS parked.
#
# WHY THIS LEG EXISTS. `chert.us/requested-at` is documented as the
# ladder's input — leg 11 of operator-kind-e2e says so in as many words,
# "whatever wants this share awake stamps it… deliberately NOT any
# particular caller's contract". So a client may stamp a share that is
# already up, and an ensure-live front door is exactly such a client.
# Nothing clears a stamp on a RUNNING share: the clear happens only on
# the transition INTO Active, and a share that is already Active never
# makes that transition. The stamp therefore ages in place, the share
# suspends carrying it — a stale stamp is what let it suspend — and
# `decide`'s down-branch then read the KEY'S PRESENCE as a standing
# request and woke it again on the next pass. Forge shipped that exact
# reading (X24) and every repository woke itself one pass after every
# park, on a cycle measured at 66 s.
#
# FOUR WAYS A GREEN RUN COULD BE VACUOUS, each with the leg that shuts it:
#
#  1. The probes do not work, so "0 pods" is true of everything. L1
#     requires the SAME probes to report 1 pod and replicas 1 while the
#     share is up.
#  2. The stamp was never really there, so L4 holds trivially. L3
#     asserts the stamp is STILL PRESENT at the moment the share parks —
#     that is the whole state under test.
#  3. The park held because the ladder is wedged, not because it is
#     right. L5 wakes it with a fresh stamp and requires it to serve.
#  4. A flap happened between two samples and was missed. L4 carries
#     `chert.us/idle-since`, which the operator rewrites on EVERY ladder
#     move: unchanged across the hold is the only reading that means
#     "nothing moved" rather than "nothing was seen to move". The flip
#     is one reconcile wide; on forge every 2 s sample read Active while
#     the operator log showed the pair inside a single second.
set -uo pipefail
NS=${NS:-lite-ladder}
SHARE=${SHARE:-tenant-l}
AFTER=${AFTER:-60}
PARK_WAIT=${PARK_WAIT:-360}
HOLD=${HOLD:-90}
STEP=${STEP:-5}
PASS=0; FAIL=0; INCONC=0
K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

phase()  { K get -n "$NS" flintshare "$SHARE" -o jsonpath='{.status.phase}' 2>/dev/null; }
istate() { K get -n "$NS" flintshare "$SHARE" -o jsonpath='{.metadata.annotations.chert\.us/idle-state}' 2>/dev/null; }
isince() { K get -n "$NS" flintshare "$SHARE" -o jsonpath='{.metadata.annotations.chert\.us/idle-since}' 2>/dev/null; }
stamp()  { K get -n "$NS" flintshare "$SHARE" -o jsonpath='{.metadata.annotations.chert\.us/requested-at}' 2>/dev/null; }
repl()   { K get -n "$NS" deploy "$SHARE" -o jsonpath='{.spec.replicas}' 2>/dev/null; }
pods()   { K get -n "$NS" pod -l app.kubernetes.io/instance="$SHARE" --no-headers 2>/dev/null | grep -c .; }

wait_phase() { local want=$1 budget=$2 t=0
  while [ "$t" -lt "$budget" ]; do [ "$(phase)" = "$want" ] && return 0; sleep "$STEP"; t=$((t+STEP)); done; return 1; }
wait_park() { local budget=$1 t=0
  while [ "$t" -lt "$budget" ]; do [ "$(repl)" = "0" ] && [ "$(pods)" = "0" ] && return 0; sleep "$STEP"; t=$((t+STEP)); done; return 1; }

echo "== L0: a tier-off share with the suspend rung armed =="
# NO bucket on purpose: `rpoClean` gates HIBERNATE, not suspend, so the
# rung under test needs no object store, no credential and no IAM at
# all. Fewer moving parts between the assertion and the mechanism.
K create namespace "$NS" --dry-run=client -o yaml | K apply -f - >/dev/null
K apply -f - >/dev/null <<EOF
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $SHARE, namespace: $NS }
spec:
  persistence: { size: 1Gi, storageClassName: local-path }
  # MONITORING IS NOT OPTIONAL FOR THE LADDER, and the first run of this
  # drill found that out: without it the hub publishes no /status, the
  # operator's poll fails, and the two-signal AND holds the share awake
  # forever: HubReachable=False PollFailed, IdleEligible=False Held.
  # (No backticks in this block -- the heredoc is unquoted so that
  # \$SHARE and \$AFTER expand, which means a backtick would be run as a
  # command. The first version of this comment did exactly that.)
  # That is the ladder's most important safety property doing
  # its job (an unreachable hub is an unknown hub, never an idle one),
  # so the drill has to give it a hub it can actually observe.
  monitoring: { enabled: true }
  idle: { suspendAfterSecs: $AFTER }
EOF
wait_phase Ready 420 || { bad "the share never reached Ready (phase $(phase))"; K get -n "$NS" flintshare "$SHARE" -o yaml | tail -25; exit 1; }
ok "the share is Ready"

echo "== L1: the probes report a LIVE share (the control arm) =="
n=$(pods); r=$(repl); s=$(istate)
[ "$n" = "1" ] && ok "the pod probe counts the live pod: $n" \
  || bad "the pod probe says $n with the share up — every park assertion below would be vacuous"
[ "$r" = "1" ] && ok "the replica probe reads 1" || bad "the replica probe reads '$r'"
case "$s" in Active|"") ok "the ladder reads Active (${s:-absent, which IS Active})" ;;
             *) bad "the ladder reads '$s' on a share that has never parked" ;; esac

echo "== L2: a client stamps a share that is ALREADY RUNNING =="
# Exactly what the CRD's contract allows and an ensure-live front door
# does. Nothing in the operator will ever clear this: the clear runs on
# the TRANSITION into Active, and this share is Active already.
asked=$(date -u +%FT%TZ)
K annotate -n "$NS" flintshare "$SHARE" "chert.us/requested-at=$asked" --overwrite >/dev/null
sleep "$STEP"
[ "$(stamp)" = "$asked" ] && ok "stamped while running: $asked" || bad "the stamp did not take"
[ "$(repl)" = "1" ] && ok "and it stays up while the stamp is fresh (signal one holds it)" \
  || bad "it came down with a FRESH request outstanding — replicas=$(repl)"

# THE MOVE COUNTER. Lite's defect is a ONE-SHOT flap, not forge's
# permanent cycle: the bogus wake CLEARS the stamp, so the share settles
# afterwards and a drill that waits for a park and then inspects it is
# looking at the SECOND park, by which time every trace is gone. The
# first version of this drill did exactly that and scored the control
# arm as a pass with a caveat.
#
# `chert.us/idle-since` is rewritten on EVERY ladder move, so the COUNT
# of distinct values from the stamp onward is the oracle, and it does
# not care that the Suspended->Active flip is one reconcile wide and
# unsamplable. One move is a share that parked and stayed. Two or more
# is a share that parked, was woken by its own stale stamp, and parked
# again.
MOVES=$(mktemp)
( while :; do isince >> "$MOVES"; echo >> "$MOVES"; sleep 2; done ) &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null' EXIT

echo "== L3: the client stops asking; the stamp goes stale and it parks =="
if wait_park "$PARK_WAIT"; then
  ok "parked: replicas 0, no pods (phase $(phase))"
else
  bad "it never parked within ${PARK_WAIT}s — nothing below measures anything"
  echo "  RESULT  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC"; exit 1
fi
carried=$(stamp)
if [ -n "$carried" ]; then
  ok "and it parked CARRYING the stale stamp ($carried) — the state under test"
else
  inconc "the stamp is gone at park time, so the hold below tests nothing about it"
fi

echo "== L4: and it STAYS parked (${HOLD}s, sampled AND clock-checked) =="
since0=$(isince); note "idle-since at the start of the hold: ${since0:-<unset>}"
flip=""; t=0
while [ "$t" -lt "$HOLD" ]; do
  st=$(istate); r=$(repl); n=$(pods)
  if [ "$st" != "Suspended" ] || [ "$r" != "0" ] || [ "$n" != "0" ]; then
    flip="at +${t}s: idle-state=$st replicas=$r pods=$n phase=$(phase)"; break
  fi
  sleep "$STEP"; t=$((t+STEP))
done
since1=$(isince)
if [ -n "$since0" ] && [ "$since1" != "$since0" ]; then
  bad "THE LADDER MOVED DURING THE HOLD without a sample catching it: idle-since $since0 -> $since1"
elif [ -z "$since0" ]; then
  inconc "idle-since was never set, so the clock oracle could not run"
else
  ok "idle-since is unchanged across the hold ($since1) — no ladder move was missed between samples"
fi
[ -z "$flip" ] && ok "the park held for ${HOLD}s across $((HOLD/STEP)) samples" \
               || bad "THE PARK DID NOT HOLD — $flip"

{ kill $SAMPLER; wait $SAMPLER; } 2>/dev/null; trap - EXIT
moves=$(grep -c . "$MOVES" 2>/dev/null || echo 0)
distinct=$(sort -u "$MOVES" 2>/dev/null | grep -c . || echo 0)
note "ladder moves observed from the stamp onward: $distinct distinct idle-since value(s) over $moves samples"
if [ "$distinct" -le 1 ]; then
  ok "the ladder moved ONCE — it parked and stayed parked"
else
  bad "THE LADDER MOVED $distinct TIMES between the stamp and the end of the hold. A share that
       parks, is woken by the stale stamp that PARKED it, and parks again reads as settled
       afterwards precisely because the bogus wake consumed the stamp:
       $(sort -u "$MOVES" | grep . | tr '\n' ' ')"
fi
rm -f "$MOVES"

echo "== L5: a FRESH request still wakes it, and the wake clears the stamp =="
K annotate -n "$NS" flintshare "$SHARE" "chert.us/requested-at=$(date -u +%FT%TZ)" --overwrite >/dev/null
if wait_phase Ready 300; then
  ok "a fresh stamp woke it — the hold above was not a wedged ladder"
else
  bad "a fresh request did not wake it (phase $(phase))"
fi
after=$(stamp)
[ -z "$after" ] && ok "and the wake CLEARED the stamp — the invariant presence-reading rests on" \
  || bad "the stamp survived the wake ($after); the next idle window starts from a stale heartbeat"

echo "  RESULT  PASS=$PASS FAIL=$FAIL INCONCLUSIVE=$INCONC"
[ "$FAIL" = "0" ]
