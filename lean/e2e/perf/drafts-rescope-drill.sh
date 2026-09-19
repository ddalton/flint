#!/usr/bin/env bash
# The DRAFTS + RESCOPE live drill — against real S3, which is the only
# place several of these can fail.
#
# WHY LIVE AT ALL. Every one of these legs has a green local test
# against MemoryStore. That is not the same claim: MemoryStore is a
# DOUBLE, and this project has already been bitten three times by a
# double that was missing a property the real store has — most recently
# etag QUOTING, which is exactly what the drafts base-etag comparison
# turns on. S3 quotes; a double that does not hides an inverted guard,
# and one that does can hide the opposite. The legs below re-ask every
# question that depends on a store behaviour rather than on our logic:
# conditional PUT semantics, CopyObject's source/destination guards,
# what a 412 looks like, and what LIST actually returns.
#
# THE GUARD DISCIPLINE, learned the expensive way on runcs, where TWO of
# my own controls came back vacuous:
#
#   - Every refusal leg carries a POSITIVE arm that must SUCCEED. A leg
#     that only ever asserts failure passes against a verb that is
#     broken in every direction, and passes loudest when it is.
#   - No leg may read an ERROR as a legal value. `aws s3api` exiting
#     non-zero for a mistyped FLAG looks identical to S3 refusing the
#     request unless the exit code is separated from the API answer —
#     that is precisely how L2c passed while testing nothing.
#   - Every arm PRINTS which path it took. Inferring the arm from a
#     configuration knob is how L4b re-ran the arm it meant to avoid.
#
# Usage:
#   ./drafts-rescope-drill.sh D1     # drafts, correctness
#   ./drafts-rescope-drill.sh D2     # rescope, correctness
#   ./drafts-rescope-drill.sh all
set -uo pipefail
cd "$(dirname "$0")"

: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${FLINT_SYNC_BIN:=./flint-sync}"
: "${FLINT_GW_BIN:=./flint-lean-gateway}"
: "${DRILL_ROOT:=/mnt/nvme/dr}"
: "${AWS_REGION:=us-west-1}"
GW_TOKEN="drill-bearer-0123456789abcdef"
GW_PORT=18080
GW="http://127.0.0.1:$GW_PORT/lean/v1/ws"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "   PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); echo "   FAIL  $*"; }
leg()  { echo; echo "== $*"; }

# An HTTP status, SEPARATED from curl's own exit code. A curl that
# cannot connect must never be readable as "the server refused me" —
# that is the error-returning-a-legal-value shape, and it is the one
# this drill is most likely to reproduce by accident.
http() { # <method> <url> [curl args...]
  local m="$1" u="$2"; shift 2
  local body code rc
  body=$(curl -sS -o /tmp/hbody -w '%{http_code}' -X "$m" \
           -H "authorization: Bearer $GW_TOKEN" "$u" "$@" 2>/tmp/herr)
  rc=$?
  if [ $rc -ne 0 ]; then
    echo "TRANSPORT-FAILURE rc=$rc $(head -c 200 /tmp/herr)"
    return 9
  fi
  echo "$body"
}
hbody() { cat /tmp/hbody; }

s3key() { aws s3api head-object --bucket "$DRILL_BUCKET" --key "$1" \
            --region "$AWS_REGION" >/dev/null 2>&1; }
s3get() { aws s3api get-object --bucket "$DRILL_BUCKET" --key "$1" \
            --region "$AWS_REGION" /tmp/s3obj >/dev/null 2>&1 && cat /tmp/s3obj; }
s3etag() { aws s3api head-object --bucket "$DRILL_BUCKET" --key "$1" \
            --region "$AWS_REGION" --query ETag --output text 2>/dev/null; }
s3count() { aws s3api list-objects-v2 --bucket "$DRILL_BUCKET" --prefix "$1" \
            --region "$AWS_REGION" --query 'length(Contents)' --output text 2>/dev/null; }

fresh_ws() { # <prefix> -> a checked-out workspace at $DRILL_ROOT/tree
  PREFIX="$1"
  rm -rf "$DRILL_ROOT/tree"; mkdir -p "$DRILL_ROOT/tree"
  export FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$PREFIX"
  export FLINT_SYNC_ROOT="$DRILL_ROOT/tree" AWS_REGION
  "$FLINT_SYNC_BIN" checkout >/tmp/co.log 2>&1 || { cat /tmp/co.log; return 1; }
}

start_gw() { # <prefix>
  pkill -f flint-lean-gateway 2>/dev/null; sleep 1
  FLINT_LEAN_GW_TOKEN="$GW_TOKEN" FLINT_LEAN_GW_BUCKET="$DRILL_BUCKET" \
  FLINT_LEAN_GW_WORKSPACES="ws=$1" FLINT_LEAN_GW_LISTEN="127.0.0.1:$GW_PORT" \
  AWS_REGION="$AWS_REGION" "$FLINT_GW_BIN" >/tmp/gw.log 2>&1 &
  for _ in $(seq 1 40); do
    curl -sf "http://127.0.0.1:$GW_PORT/healthz" >/dev/null && return 0
    sleep 0.25
  done
  echo "gateway never came up:"; tail -20 /tmp/gw.log; return 1
}

# ─────────────────────────────────────────────────────────────────────
D1() {
  local P="drill/drafts-$(date -u +%s)"
  leg "D1 — drafts against real S3 [prefix $P]"
  fresh_ws "$P" || { bad "D1 setup"; return; }
  echo "published v1" > "$DRILL_ROOT/tree/inputs.txt"
  echo "sibling file" > "$DRILL_ROOT/tree/other.txt"
  "$FLINT_SYNC_BIN" barrier >/tmp/b.log 2>&1 || { cat /tmp/b.log; bad "D1 seed barrier"; return; }
  start_gw "$P" || { bad "D1 gateway"; return; }

  local base; base=$(s3etag "$P/files/inputs.txt")
  echo "   base etag from S3: $base"

  # D1a — a save is durable and changes NOTHING live.
  local code
  code=$(http PUT "$GW/drafts/alice/inputs.txt" \
           -H "x-flint-base-etag: $base" --data-binary "alice's draft")
  [ "$code" = 200 ] && ok "D1a save accepted" || bad "D1a save -> $code $(hbody)"
  s3key "$P/.flint/lean/drafts/alice/body/inputs.txt" \
    && ok "D1a body durable in S3" || bad "D1a body missing"
  s3key "$P/.flint/lean/drafts/alice/meta/inputs.txt" \
    && ok "D1a meta durable in S3" || bad "D1a meta missing"
  [ "$(s3get "$P/files/inputs.txt")" = "published v1" ] \
    && ok "D1a live object UNTOUCHED" || bad "D1a the save went live"

  # D1b — invisible across two barriers (one could pass wrongly).
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  [ "$(cat "$DRILL_ROOT/tree/inputs.txt")" = "published v1" ] \
    && ok "D1b draft never reached the agent tree" || bad "D1b draft leaked into the tree"
  # THE POSITIVE CONTROL: the same bytes through the HITL door DO land.
  code=$(http PUT "$GW/files/other.txt" -H "if-match: $(s3etag "$P/files/other.txt")" \
           --data-binary "hitl wrote this")
  [ "$code" = 200 ] && ok "D1b control: HITL write accepted" || bad "D1b control -> $code"
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  [ "$(cat "$DRILL_ROOT/tree/other.txt")" = "hitl wrote this" ] \
    && ok "D1b control: HITL DID reach the tree (so D1b means something)" \
    || bad "D1b control never landed — D1b proves nothing"

  # D1c — the two-user case, the whole point of the feature.
  # A REAL FILE, never process substitution: `aws s3api put-object`
  # seeks its body, and /dev/fd/NN is not seekable — so `--body <(...)`
  # fails, the sibling write never lands, and every assertion after it
  # reads the ABSENCE of a conflict as the product being wrong. The
  # guard below caught exactly that on the first live run.
  printf 'bob published over alice\n' > /tmp/bob.txt
  aws s3api put-object --bucket "$DRILL_BUCKET" --key "$P/files/inputs.txt" \
    --body /tmp/bob.txt --region "$AWS_REGION" >/dev/null 2>&1 \
    || { echo "sibling write failed"; }
  local bob; bob=$(s3etag "$P/files/inputs.txt")
  [ "$bob" != "$base" ] && ok "D1c sibling moved the object ($base -> $bob)" \
    || bad "D1c sibling write did not move it — the rest of D1c is vacuous"
  code=$(http POST "$GW/drafts/alice/inputs.txt")
  [ "$code" = 409 ] && ok "D1c stale promote REFUSED 409" || bad "D1c promote -> $code $(hbody)"
  grep -q 'draft-stale' /tmp/hbody && ok "D1c named draft-stale" || bad "D1c wrong error: $(hbody)"
  [ "$(s3get "$P/files/inputs.txt")" = "bob published over alice" ] \
    && ok "D1c bob's bytes stand" || bad "D1c alice clobbered bob"
  code=$(http GET "$GW/drafts/alice/inputs.txt")
  [ "$code" = 200 ] && [ "$(hbody)" = "alice's draft" ] \
    && ok "D1c THE DRAFT IS KEPT" || bad "D1c a refusal discarded the draft"

  # D1d — re-save against what is there now, then promote succeeds.
  # This is D1c's positive arm: without it, "promote refuses" would pass
  # against a promote that can never succeed.
  code=$(http PUT "$GW/drafts/alice/inputs.txt" \
           -H "x-flint-base-etag: $bob" --data-binary "alice rebased")
  [ "$code" = 200 ] && ok "D1d re-save accepted" || bad "D1d re-save -> $code"
  code=$(http POST "$GW/drafts/alice/inputs.txt")
  [ "$code" = 200 ] && ok "D1d promote SUCCEEDS once rebased" || bad "D1d promote -> $code $(hbody)"
  [ "$(s3get "$P/files/inputs.txt")" = "alice rebased" ] \
    && ok "D1d live object is the draft" || bad "D1d promote did not publish"
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  [ "$(cat "$DRILL_ROOT/tree/inputs.txt")" = "alice rebased" ] \
    && ok "D1d barrier consumed the promote" || bad "D1d promote never consumed"
  code=$(http GET "$GW/drafts/alice")
  grep -q '"drafts":\[\]' /tmp/hbody \
    && ok "D1d the draft is gone after promote" || bad "D1d draft survived: $(hbody)"

  # D1e — an INCOMPLETE draft refuses, and publishes nothing.
  http PUT "$GW/drafts/alice/other.txt" --data-binary "incomplete" >/dev/null
  aws s3api delete-object --bucket "$DRILL_BUCKET" \
    --key "$P/.flint/lean/drafts/alice/meta/other.txt" --region "$AWS_REGION" >/dev/null
  local before; before=$(s3get "$P/files/other.txt")
  code=$(http POST "$GW/drafts/alice/other.txt")
  [ "$code" = 404 ] && ok "D1e incomplete draft refuses promote" || bad "D1e -> $code $(hbody)"
  [ "$(s3get "$P/files/other.txt")" = "$before" ] \
    && ok "D1e nothing published" || bad "D1e an incomplete draft published"
}

# ─────────────────────────────────────────────────────────────────────
D2() {
  local P="drill/rescope-$(date -u +%s)"
  leg "D2 — rescope against real S3 [prefix $P]"
  fresh_ws "$P" || { bad "D2 setup"; return; }
  mkdir -p "$DRILL_ROOT/tree/inputs" "$DRILL_ROOT/tree/outputs"
  echo "keep me" > "$DRILL_ROOT/tree/inputs/a.txt"
  for i in 0 1 2 3 4 5; do echo "out-$i" > "$DRILL_ROOT/tree/outputs/o-$i.txt"; done
  "$FLINT_SYNC_BIN" barrier >/tmp/b.log 2>&1 || { cat /tmp/b.log; bad "D2 seed"; return; }
  local n0; n0=$(s3count "$P/files/")
  [ "$n0" = 7 ] && ok "D2 seeded 7 objects" || bad "D2 seeded $n0, expected 7"

  # D2a — the narrow: six files leave, ZERO deletions published.
  "$FLINT_SYNC_BIN" rescope inputs > /tmp/rs.log 2>&1 || { cat /tmp/rs.log; bad "D2a rescope"; return; }
  grep -E 'uncited|unlinked' /tmp/rs.log | sed 's/^/   /'
  [ ! -e "$DRILL_ROOT/tree/outputs/o-0.txt" ] && ok "D2a narrow unlinked" || bad "D2a still on disk"
  [ -e "$DRILL_ROOT/tree/inputs/a.txt" ] && ok "D2a admitted path kept" || bad "D2a took an admitted path"
  "$FLINT_SYNC_BIN" barrier >/tmp/b1.log 2>&1
  "$FLINT_SYNC_BIN" barrier >/tmp/b2.log 2>&1
  local n1; n1=$(s3count "$P/files/")
  [ "$n1" = 7 ] && ok "D2a TWO barriers, bucket still 7 — no deletion published" \
                || bad "D2a bucket went $n0 -> $n1: a narrow published deletions"

  # D2b — THE CONTROL. The same files removed WITHOUT the narrow must
  # publish their deletions. Without this arm, D2a passes against a
  # delete rule that never bites, which is the exact shape of the two
  # vacuous guards runcs produced.
  "$FLINT_SYNC_BIN" rescope --all > /tmp/rs2.log 2>&1 || { cat /tmp/rs2.log; bad "D2b widen"; return; }
  local n2; n2=$(s3count "$P/files/")
  [ "$n2" = 7 ] && ok "D2b widen restored the held set (bucket still 7)" || bad "D2b bucket $n2"
  [ -e "$DRILL_ROOT/tree/outputs/o-0.txt" ] && ok "D2b widen re-materialised" || bad "D2b widen fetched nothing"
  rm -f "$DRILL_ROOT/tree/outputs"/o-*.txt
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  "$FLINT_SYNC_BIN" barrier >/dev/null 2>&1
  local n3; n3=$(s3count "$P/files/")
  [ "$n3" = 1 ] && ok "D2b CONTROL: a plain rm DID publish 6 deletions (7 -> 1)" \
                || bad "D2b control bucket=$n3 — the delete rule never bit, so D2a is vacuous"
}

case "${1:-all}" in
  D1) D1 ;;
  D2) D2 ;;
  all) D1; D2 ;;
  *) echo "usage: $0 D1|D2|all"; exit 2 ;;
esac
echo
echo "drafts+rescope drill: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
