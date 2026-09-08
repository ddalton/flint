#!/usr/bin/env bash
# THE POSITIVE CONTROL for F14's P8 — replay a bucket that is KNOWN to be
# unrestorable and require the drill to say so.
#
#   BUCKET=... PREFIX=... CORRUPT_DIR=<preserved prefix> \
#     ./forge/e2e/f14-corrupt-control.sh
#
# WHY THIS EXISTS. P8 asks "did the repository survive, and does it come
# back from S3 alone?" — and a leg that only ever sees healthy buckets
# cannot be trusted to notice an unhealthy one. It would have passed
# every run before 2026-09-08, including the one whose bucket was already
# corrupt. So this seeds a bucket that IS corrupt and requires the
# refusal, through exactly the path P8 uses: a fresh syncer restoring
# from S3 and running `fsck --connectivity-only`.
#
# CORRUPT_DIR is a drill artifact, not a fixture in this repository: the
# `<prefix>/<repo>/` tree preserved from a run that produced the state.
# runcj's is ~332 objects / 1.3 MB. Any preserved prefix works — that is
# the point, since the next such defect will have its own.
#
# EXPECTED: the repository NEVER reaches Ready, and the syncer says
# `refused:` naming a missing object. A repository that comes up Ready
# here is the failure — it means the oracle cannot tell the two apart.
set -uo pipefail
NS=${NS:-agents}
: "${BUCKET:?}"; : "${PREFIX:?}"; : "${CORRUPT_DIR:?set CORRUPT_DIR to a preserved <prefix>/<repo>/ tree}"
REPO=${REPO:-f14corrupt}
TAG=${TAG:?set TAG to the image tag this drill deployed}
WAIT=${WAIT:-180}
PASS=0; FAIL=0

K() { kubectl "$@"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
note() { printf '  ....  %s\n' "$*"; }

[ -d "$CORRUPT_DIR/git" ] || { echo "no git/ under $CORRUPT_DIR — is that a preserved repository prefix?"; exit 2; }
n=$(find "$CORRUPT_DIR" -type f | wc -l | tr -d ' ')
note "seeding $n preserved objects into s3://$BUCKET/$PREFIX/$REPO/"
aws s3 sync "$CORRUPT_DIR" "s3://$BUCKET/$PREFIX/$REPO/" >/dev/null 2>&1 \
  || { echo "the seed upload failed; the control cannot run"; exit 2; }
# The seed must actually be there, or "it never came up" would be true of
# an empty prefix and would prove nothing.
got=$(aws s3 ls "s3://$BUCKET/$PREFIX/$REPO/" --recursive 2>/dev/null | wc -l | tr -d ' ')
[ "${got:-0}" -ge "$n" ] && ok "the corrupt state is in the bucket ($got objects)" \
  || { bad "only ${got:-0} of $n objects reached the bucket"; exit 1; }

sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" forge/e2e/f14-repo.yaml.tpl | K apply -f - >/dev/null

phase=""; refused=""
for _ in $(seq 1 $((WAIT / 5))); do
  phase=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null)
  pod=$(K get pods -n "$NS" -o name 2>/dev/null | grep "forge-$REPO" | head -1)
  if [ -n "$pod" ]; then
    log=$(K logs -n "$NS" "$pod" -c syncer --tail=20 2>/dev/null; K logs -n "$NS" "$pod" -c syncer --previous --tail=20 2>/dev/null)
    printf '%s' "$log" | grep -q "refused:" && { refused=$(printf '%s' "$log" | grep -o 'refused:.*' | head -1); break; }
  fi
  [ "$phase" = "Ready" ] && break
  sleep 5
done

note "phase after ${WAIT}s: ${phase:-none}"
if [ -n "$refused" ]; then
  ok "the syncer REFUSED the corrupt bucket rather than serving it"
  note "$(printf '%s' "$refused" | cut -c1-160)"
  printf '%s' "$refused" | grep -qE "missing (commit|object|blob|tree)|broken link" \
    && ok "and it named the missing object — the oracle discriminates" \
    || bad "the refusal does not name a missing object: $refused"
else
  bad "no refusal was logged"
fi
[ "$phase" = "Ready" ] \
  && bad "the corrupt repository reached READY — P8's oracle cannot tell a broken bucket from a whole one" \
  || ok "the corrupt repository never reached Ready"

K delete -n "$NS" flintrepo "$REPO" --wait=false >/dev/null 2>&1
echo
echo "F14-control: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
