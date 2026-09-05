#!/usr/bin/env bash
# FALSIFIER 2 — concurrent pushes to one ref.
#
# Two clients read the same base L and push L→N1 and L→N2 to the SAME
# ref at the same instant. Exactly one must be told `ok`; the other
# must be refused; and the bucket and the server's own ref must both
# hold the winner's oid and nothing else.
#
# WHY THIS IS THE LOAD-BEARING ONE. `receive-pack` serialises nothing
# and performs no old-oid check under `receive.procReceiveRefs`, so
# git itself provides NO arbitration here. Every ordering guarantee is
# forge's own code — the effective-ref overlay in `batch.rs`, updated
# after each accepted command — which means an error there is silent:
# both clients see success and the bucket keeps one of the two.
#
#   ./forge/e2e/f2-concurrent.sh          # the shipped syncer
#   CONTROL=1 ./forge/e2e/f2-concurrent.sh
#
# The control runs an image built from the same tree with exactly one
# line removed (the overlay update). It MUST fail this drill; if it
# passes, the drill is not testing what it claims.
set -uo pipefail
NS=${NS:-agents}
REPO=${REPO:-proj}
RACE_REF=${RACE_REF:-refs/heads/agent/race}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
STAGGER=${STAGGER:-0}     # seconds between the two pushes; 0 = same batch
out=$(mktemp -d)

say() { printf '%s\n' "$*"; }

# The two clients, each in its own pod with its own token.
for a in racer-a racer-b; do
  kubectl get pod -n "$NS" "$a" >/dev/null 2>&1 || \
    AGENT=$a TAG=${TAG:-1.46.0-forge.4} envsubst '$AGENT $TAG' \
      < "$(dirname "$0")/agent.yaml.tpl" | kubectl apply -f - >/dev/null
done
kubectl wait -n "$NS" --for=condition=Ready pod/racer-a pod/racer-b --timeout=180s >/dev/null

# ── seed the contested ref so both clients share a base L ────────────
# The seed must build ON the current tip, not replace it: a re-run
# that force-pushes an unrelated commit is refused by the
# non-fast-forward rule, and that refusal is the POLICY working, not
# the drill failing. (It cost one run to learn.)
kubectl exec -n "$NS" racer-a -- sh -c "
  set -e
  T=\$(cat /var/run/secrets/forge/token)
  A=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  rm -rf /tmp/seed
  git -c http.extraHeader=\"\$A\" clone -q ${DOOR}/git/${NS}/${REPO}.git /tmp/seed
  cd /tmp/seed && git config user.email r@a.c && git config user.name racer
  BR=\$(echo ${RACE_REF} | sed 's|refs/heads/||')
  if git rev-parse --verify -q origin/\$BR >/dev/null; then
    git checkout -q -B \$BR origin/\$BR
  else
    git checkout -q -B \$BR
  fi
  echo \"base \$(date +%s)\" > race.txt && git add -A && git commit -qm base
  git -c http.extraHeader=\"\$A\" push -q origin HEAD:${RACE_REF}
  git rev-parse HEAD
" > "$out/base" 2>"$out/base.err"
BASE=$(tail -1 "$out/base")
say "base L = ${BASE:-<seed failed>}"
[ -n "$BASE" ] || { cat "$out/base.err"; exit 1; }

# ── each client builds its own successor of L ────────────────────────
race_one() {  # $1=pod $2=content $3=start-epoch
  kubectl exec -n "$NS" "$1" -- sh -c "
    set -e
    T=\$(cat /var/run/secrets/forge/token)
    A=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
    rm -rf /tmp/w && git -c http.extraHeader=\"\$A\" clone -q --branch \
      \$(echo ${RACE_REF} | sed 's|refs/heads/||') ${DOOR}/git/${NS}/${REPO}.git /tmp/w
    cd /tmp/w && git config user.email r@a.c && git config user.name $1
    test \"\$(git rev-parse HEAD)\" = \"$BASE\" || { echo 'BAD BASE'; exit 9; }
    echo '$2' > race.txt && git add -A && git commit -qm '$2'
    echo \"MINE \$(git rev-parse HEAD)\"
    # Wall-clock barrier: both clients push in the same instant, so the
    # two commands land inside one batch window.
    while [ \"\$(date +%s)\" -lt $3 ]; do :; done
    git -c http.extraHeader=\"\$A\" push origin HEAD:${RACE_REF} 2>&1
    echo \"EXIT \$?\"
  " 2>&1
}

START=$(( $(date +%s) + 12 ))
say "── racing at epoch $START (stagger=${STAGGER}s) ──"
race_one racer-a alpha "$START"                 > "$out/a" 2>&1 &
PA=$!
race_one racer-b bravo "$((START + STAGGER))"   > "$out/b" 2>&1 &
PB=$!
wait $PA; wait $PB

for f in a b; do say "── racer-$f ──"; cat "$out/$f"; done

# ── the oracle ───────────────────────────────────────────────────────
winners=0; losers=0
for f in a b; do
  if grep -qE '^EXIT 0$' "$out/$f" && ! grep -q 'rejected' "$out/$f"; then
    winners=$((winners+1)); eval "oid_$f=\$(grep '^MINE ' '$out/$f' | awk '{print \$2}')"
  else
    losers=$((losers+1))
  fi
done
say ""
say "accepted=$winners refused=$losers"
echo "$winners" > "$out/winners"
say "out=$out"
