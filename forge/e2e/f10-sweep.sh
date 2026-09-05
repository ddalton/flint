#!/usr/bin/env bash
# FALSIFIER 10 — the sweep.
#
# After a repack, old packs are deleted past the grace; a pack the
# snapshot names is NEVER deleted; and the probe asserts the sweep
# actually fired rather than inferring it from an absence.
#
# The asymmetry is the whole point. Deleting an orphan too early is a
# repository that cannot be restored; keeping every orphan forever is a
# bill. Only one of those is a correctness bug, so the sweep is built to
# fail towards the bill.
set -uo pipefail
NS=${NS:-agents}; REPO=${REPO:-proj}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-drill}
PUSHES=${PUSHES:-20}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }
snap(){ aws s3 cp "s3://$BUCKET/$PREFIX/git/git/snapshot" - 2>/dev/null; }
bucket_packs(){ aws s3 ls "s3://$BUCKET/$PREFIX/git/git/objects/pack/" | awk '{print $4}' | grep '\.pack$' | sort; }

echo "══ F10: repack, then sweep ══"
P0=$(snap | jq -r '.packs | length')
echo "packs named by the snapshot: $P0   (repack threshold is 24)"

# A REPACK LEAVES NO LOG LINE, so it cannot be found after the fact:
# by the time the pushes finish, the consolidation has already happened
# and the count has climbed again. The first run of this drill polled
# afterwards, saw 5 packs, and called it "no repack observed" — while a
# repack had in fact run. So sample DURING.
echo "── $PUSHES pushes (one pack each), sampling the snapshot throughout ──"
SAMPLES=$(mktemp)
( while :; do snap | jq -r '.packs | length' >> "$SAMPLES" 2>/dev/null; sleep 2; done ) &
SAMPLER=$!

kubectl exec -n "$NS" agent1 -- sh -c "
  T=\$(cat /var/run/secrets/forge/token); H=\"Authorization: Basic \$(printf 'x:%s' \"\$T\" | base64 -w0)\"
  G(){ git -c http.$DOOR/.extraHeader=\"\$H\" \"\$@\"; }
  rm -rf /tmp/sw && G clone -q $DOOR/git/$NS/$REPO.git /tmp/sw && cd /tmp/sw
  git config user.email s@x.y; git config user.name sweeper
  # CONTINUE the existing branch. Branching from main instead makes
  # every push a non-fast-forward, which the policy correctly refuses —
  # and a drill whose pushes are all silently refused reports a pack
  # count that never moves and blames the repack. That happened.
  if git rev-parse --verify -q origin/agent/sweep >/dev/null; then
    git checkout -q -B agent/sweep origin/agent/sweep
  else
    git checkout -q -B agent/sweep
  fi
  i=0; okc=0; while [ \$i -lt $PUSHES ]; do
    echo \"pack \$i \$(date +%s%N)\" > sweep.txt
    git add -A && git commit -qm \"p\$i\" >/dev/null
    if G push -q origin HEAD:refs/heads/agent/sweep 2>/dev/null; then okc=\$((okc+1)); fi
    i=\$((i+1))
  done
  echo \"PUSHED=\$okc\" > /tmp/sw.count" >/dev/null 2>&1
PUSHED=$(kubectl exec -n "$NS" agent1 -- sh -c 'sed -n "s/^PUSHED=//p" /tmp/sw.count' 2>/dev/null)
[ "${PUSHED:-0}" = "$PUSHES" ] && ok "all $PUSHES pushes were accepted" \
  || { bad "only ${PUSHED:-0}/$PUSHES pushes landed — nothing below this measures the repack"; }
sleep 8
kill $SAMPLER 2>/dev/null; wait $SAMPLER 2>/dev/null

PEAK=$(sort -n "$SAMPLES" | tail -1); FINAL=$(tail -1 "$SAMPLES")
echo "   snapshot pack count: start $P0, peak $PEAK, final $FINAL  (threshold 24)"
if [ "${PEAK:-0}" -gt 24 ] && [ "${FINAL:-0}" -lt "${PEAK:-0}" ]; then
  ok "a repack fired: the count crossed the threshold ($PEAK) and then collapsed to $FINAL"
else
  bad "no repack observed: peak $PEAK, final $FINAL — the count never crossed 24 and fell"
fi
rm -f "$SAMPLES"

# THE SAFETY INVARIANT: every pack the snapshot names must exist.
MISSING=0
for p in $(snap | jq -r '.packs[]'); do
  aws s3api head-object --bucket "$BUCKET" --key "$PREFIX/git/git/objects/pack/$p" >/dev/null 2>&1 || { MISSING=$((MISSING+1)); echo "      MISSING: $p"; }
done
[ "$MISSING" = 0 ] && ok "every pack the snapshot names is present in the bucket" || bad "$MISSING snapshot pack(s) missing — the repository cannot be restored"

# THE SWEEP'S REAL INVARIANT, not its log line. A run whose orphans are
# all younger than the grace sweeps NOTHING, and that is correct — so
# demanding a log line marks correct behaviour as a failure, which is
# what the first version of this check did. What must hold is:
#
#   every pack object in the bucket is either named by the snapshot,
#   or younger than the grace.
#
# Anything else is a leak that will never be collected.
GRACE=${GRACE:-3600}
NOW=$(date -u +%s)
KEEP=$(snap | jq -r '.packs[]' | sed 's/\.pack$//' | sort -u)
LEAKED=0; ORPHANS=0
while read -r ts _ sz key; do
  [ -z "$key" ] && continue
  base=$(basename "$key"); stem=${base%.*}
  echo "$KEEP" | grep -qx "$stem" && continue
  ORPHANS=$((ORPHANS+1))
  age=$(( NOW - $(date -u -j -f '%Y-%m-%d %H:%M:%S' "$ts" +%s 2>/dev/null || echo "$NOW") ))
  if [ "$age" -gt "$((GRACE + 600))" ]; then
    LEAKED=$((LEAKED+1)); echo "      LEAKED: $base (${age}s old, grace ${GRACE}s)"
  fi
done <<EOF
$(aws s3 ls "s3://$BUCKET/$PREFIX/git/git/objects/pack/" | awk '{print $1" "$2" "$3" "$4}')
EOF
[ "$LEAKED" = 0 ] && ok "no orphan older than the grace survives ($ORPHANS orphan(s), all within it)" \
  || bad "$LEAKED orphan pack file(s) are past the grace and were not swept"
# Informational: the sweep does say so when it has something to take.
kubectl logs -n "$NS" deploy/forge-proj -c syncer --tail=600 2>/dev/null | grep -i 'swept' | tail -2 | sed 's/^/      (log) /'

# A cold restore is the real proof the sweep did not take something live.
OLD=$(kubectl get pods -n "$NS" -l chert.us/repo=proj -o jsonpath='{.items[0].metadata.name}')
kubectl delete pod -n "$NS" "$OLD" --wait=true >/dev/null
kubectl wait -n "$NS" --for=condition=Available deploy/forge-proj --timeout=300s >/dev/null
kubectl exec -n "$NS" deploy/forge-proj -c syncer -- git --git-dir=/repo/$NS/$REPO.git fsck --strict >/dev/null 2>&1 \
  && ok "cold restore after the repack+sweep passes fsck --strict" || bad "restore after the sweep is broken"
echo ""; echo "══ $pass passed, $fail failed ══"
