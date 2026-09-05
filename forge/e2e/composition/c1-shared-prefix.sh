#!/usr/bin/env bash
# C1 — a forge repository and a lean workspace pointed at ONE prefix.
#
# THE CLAIM UNDER TEST. "One prefix has exactly one writer" is stated
# as the arbitration rule of the whole system: each prefix has one
# epoch lease and one pointer, and writes are serialised by conditional
# PUT against that pointer. The natural reading is that pointing two
# products at one prefix is SELF-LIMITING — they contend for the lease,
# one wins, the loser is fenced and says so.
#
# WHY IT MIGHT NOT BE. The two products derive the lease key
# independently:
#
#   forge  <prefix>/git/epoch          (forge/syncer/src/lib.rs:210)
#   lean   <prefix>/.flint/lean/epoch  (lean/sidecar/src/lib.rs:389)
#
# If those disagree there is no contention to win: both processes
# acquire, both are correct that they hold THEIR cell, and neither can
# see the other. That failure is silent by construction — no 412, no
# fence, no log line — which is why it needs a drill rather than a
# reading.
#
# WHAT CHANGED, AND WHAT DID NOT. Nothing here prevents the collision
# — prevention belongs to whatever assigns prefixes, and enforcing it
# in-band is a design decision nobody has taken. What both products now
# do is SAY SO: each probes the other's lease cell at claim time (one
# exact-key read, `flint_store::layout`) and prints what it found. The
# legs below still FAIL, because the rule is still not enforced; the
# new legs assert that the condition is at least audible.
#
# ANTI-VACUITY. A drill that only shows "both started fine" proves
# nothing: it cannot tell disjoint cells apart from a rig too blunt to
# observe contention at all. So two controls run FIRST and must both
# show contention on this very rig — forge against forge, and lean
# against lean, on their own prefixes. Only then does the cross-product
# leg mean anything.
#
#   bash forge/e2e/composition/c1-shared-prefix.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c1}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
rig_purge c1/
rig_gate

seed_workspace() { mkdir -p "$1"; printf 'notes\n' > "$1/notes.txt"; }

# ── control 1: forge vs forge on one prefix ──────────────────────────
head_ "control 1 — two forge servers on one prefix must contend"
new_bare_repo "$WORK/f1.git"; new_bare_repo "$WORK/f2.git"
forge_up f1 "$WORK/f1.git" "c1/forge"
wait_key c1/forge/git/epoch 30 && ok "first forge server holds the lease" \
  || bad "first forge server never took the lease"
forge_up f2 "$WORK/f2.git" "c1/forge"
sleep 8
if forge_log f2 | grep -q "another server holds"; then
  ok "the second forge server is held off (contention is observable here)"
else
  bad "the second forge server did not contend — the rig cannot see contention"
  forge_log f2 | head -5
fi
forge_down f2; forge_down f1

# ── control 2: lean vs lean on one prefix ────────────────────────────
head_ "control 2 — two lean sidecars on one prefix must contend"
seed_workspace "$WORK/w1"; seed_workspace "$WORK/w2"
( lean run c1/lean "$WORK/w1" FLINT_SYNC_FLOOR_SECS=5 > "$WORK/lean1.log" 2>&1 ) &
LEAN1=$!
wait_key c1/lean/.flint/lean/epoch 40 && ok "first lean sidecar holds the lease" \
  || bad "first lean sidecar never took the lease"
( lean barrier c1/lean "$WORK/w2" > "$WORK/lean2.log" 2>&1 ) &
LEAN2=$!
for _ in $(seq 1 25); do
  grep -q "waiting on the standing lease" "$WORK/lean2.log" 2>/dev/null && break
  kill -0 $LEAN2 2>/dev/null || break
  sleep 1
done
kill $LEAN2 2>/dev/null; wait $LEAN2 2>/dev/null
if grep -q "waiting on the standing lease" "$WORK/lean2.log"; then
  ok "the second lean sidecar is held off (contention is observable here)"
else
  bad "the second lean sidecar did not contend"
  head -5 "$WORK/lean2.log"
fi
kill $LEAN1 2>/dev/null; wait $LEAN1 2>/dev/null

# ── the composition: forge and lean on ONE prefix ────────────────────
head_ "C1 — forge and lean on ONE prefix"
new_bare_repo "$WORK/x.git"; seed_workspace "$WORK/wx"
forge_up x "$WORK/x.git" "c1/shared"
wait_key c1/shared/git/epoch 30 && ok "forge took its lease on c1/shared" \
  || bad "forge never took its lease"

lean barrier c1/shared "$WORK/wx" > "$WORK/leanx.log" 2>&1
rc=$?
if [ $rc -eq 0 ]; then
  bad "lean published into forge's live prefix and was NOT refused (rc=0)"
else
  ok "lean was refused on forge's prefix (rc=$rc)"
fi
grep -q "waiting on the standing lease" "$WORK/leanx.log" \
  && ok "lean saw forge's lease" \
  || bad "lean never saw forge's lease"

# The decisive observation: what is actually IN the bucket.
head_ "C1 — the arbitration cells"
fe=$(s3_has c1/shared/git/epoch && echo yes || echo no)
le=$(s3_has c1/shared/.flint/lean/epoch && echo yes || echo no)
note "c1/shared/git/epoch         = $fe"
note "c1/shared/.flint/lean/epoch = $le"
if [ "$fe" = yes ] && [ "$le" = yes ]; then
  bad "TWO live lease cells under one prefix — the products cannot see each other"
  note "forge holder: $(s3_cat c1/shared/git/epoch | head -c 200)"
  note "lean  holder: $(s3_cat c1/shared/.flint/lean/epoch | head -c 200)"
else
  ok "only one lease cell exists under the prefix"
fi

# forge must be unharmed either way — it is still not fenced, because
# the two never contend. What is new is that it is no longer UNAWARE.
forge_log x | grep -qi "fenced\|deposed" \
  && bad "forge was fenced by the foreign writer" \
  || ok "forge was never fenced (the products still do not contend)"

head_ "C1c — is the shared prefix at least AUDIBLE?"
# lean ran second, so it saw forge's standing cell.
if grep -q "PREFIX SHARED WITH ANOTHER PRODUCT" "$WORK/leanx.log"; then
  ok "lean reported the shared prefix"
  grep -o "Its lease cell is [^ ]*" "$WORK/leanx.log" | head -1 | sed 's/^/  ....  /'
else
  bad "lean said nothing about sharing a prefix with forge"
fi

# And forge must see it on ITS next start, with lean's cell now standing.
forge_down x
forge_up x2 "$WORK/x.git" "c1/shared"
sleep 6
if forge_log x2 | grep -q "PREFIX SHARED WITH ANOTHER PRODUCT"; then
  ok "forge reported the shared prefix on its next start"
  forge_log x2 | grep -o "holder [^,]*" | head -1 | sed 's/^/  ....  /'
else
  bad "forge said nothing about sharing a prefix with lean"
fi
# The report must be usable: it has to name the cell, not just complain.
forge_log x2 | grep -q "c1/shared/.flint/lean/epoch" \
  && ok "forge named the foreign cell" \
  || bad "forge complained without naming the cell"
forge_down x2

# The control that stops this becoming a false alarm on every healthy
# deployment: a repository ALONE on its prefix must say nothing.
head_ "C1d — a writer alone on its prefix must stay quiet"
new_bare_repo "$WORK/solo.git"
forge_up solo "$WORK/solo.git" "c1/solo"
sleep 6
forge_log solo | grep -q "PREFIX SHARED" \
  && bad "a repository alone on its prefix reported a collision" \
  || ok "a repository alone on its prefix says nothing"
forge_down solo

# ── C1e: the export prefix is covered by the PUBLISHER, once ────────
# The spawned `flint-sync` skips its own probe when it publishes a
# mirror — a per-export read whose warning forge's line filter would
# discard anyway. That is only sound if the coverage moved rather than
# vanished, so this asserts the publisher does it at startup.
head_ "C1e — a foreign repository rooted on the EXPORT prefix"
new_bare_repo "$WORK/e.git"
# A forge repository squatting where our export publishes.
cat > "$WORK/squat.json" <<'JSON'
{"holder_id":"forge-squatter","epoch":1,"renewed_unix":1788600000,"salt":"s","released":false}
JSON
s3_put "c1/exp/git/epoch" "$WORK/squat.json"
forge_up e "$WORK/e.git" "c1/erepo" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=c1/exp" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
sleep 6
if forge_log e | grep -q "on the export prefix"; then
  ok "the publisher reported a foreign writer on its export prefix"
  forge_log e | grep -o "holder [^,]*" | head -1 | sed 's/^/  ....  /'
else
  bad "nobody reported a foreign writer on the export prefix"
fi
forge_down e

# ── C1b: is the self-collision guard containment or string equality? ─
head_ "C1b — export prefix NESTED under the repository's own prefix"
new_bare_repo "$WORK/n.git"
forge_up n "$WORK/n.git" "c1/nest" \
  "FLINT_FORGE_EXPORT_REF=refs/heads/main" \
  "FLINT_FORGE_EXPORT_PREFIX=c1/nest/inner" \
  "FLINT_FORGE_EXPORT_EVERY_SECS=0"
sleep 5
if forge_log n | grep -q "would be a second writer"; then
  ok "a nested export prefix is refused"
else
  bad "a nested export prefix is ACCEPTED — the guard is string equality, not containment"
  note "$(forge_log n | head -3)"
fi
forge_down n

verdict "C1"
