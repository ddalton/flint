#!/usr/bin/env bash
# Does agent-fleet-doc-drill.sh's seed_put helper do what it claims?
#
# WHY THIS EXISTS. The lite file API refuses an unconditioned PUT over a
# path that already exists with 428, so a drill that seeds a FIXED key
# is a create on its first run and a refusal on every run after. The
# drills paper over that with a helper: PUT unconditioned, and on 428
# re-issue under `If-Match: *`.
#
# That helper is three lines of shell in the middle of a drill that
# needs a cluster, a bucket and twelve minutes to reach it — so nothing
# exercises it until everything else already works. This runs it in
# isolation in under a second, and it catches the failure that costs
# most: a helper that retries on ANY code turns a real 503 into a 200
# and reports a seed that never landed as a seed that did.
#
# NO DRIFT. The helper is EXTRACTED from the drill rather than copied,
# so editing the drill's copy is what this tests. If the extraction
# finds nothing, that is a failure, not a skip — a test that silently
# tests an empty function is worse than no test.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DRILL="$ROOT/tests/regression/agent-fleet-doc-drill.sh"
[ -f "$DRILL" ] || { echo "FAIL: no drill at $DRILL"; exit 1; }

D=$(mktemp -d); trap 'rm -rf "$D"' EXIT
sed -n '/^seed_put() {$/,/^}$/p' "$DRILL" > "$D/helper.sh"
grep -q 'If-Match: \*' "$D/helper.sh" \
  || { echo "FAIL: extracted no seed_put (or one with no If-Match) from the drill"; exit 1; }

note() { echo "    · $*" >&2; }
# The stub runs inside $( ), i.e. a SUBSHELL, so its state MUST live in
# files. The first version of this harness used shell variables, saw
# every counter read back 0, and reported the helper broken when the
# harness was.
gw() {
  local n; n=$(cat "$D/calls"); n=$((n+1)); echo "$n" > "$D/calls"
  printf '%s\n' "$*" >> "$D/args"
  sed -n "${n}p" "$D/replies"
}
# shellcheck disable=SC1090
. "$D/helper.sh"

fails=0
arm()   { echo 0 > "$D/calls"; : > "$D/args"; printf '%s\n' "$@" > "$D/replies"; }
chk()   { if [ "$2" = "$3" ]; then echo "  ok  $1"; else echo "  BAD $1: got '$2' wanted '$3'"; fails=$((fails+1)); fi; }
has()   { if grep -qF -- "$2" "$D/args"; then echo "  ok  $1"; else echo "  BAD $1"; fails=$((fails+1)); fi; }
hasnt() { if grep -qF -- "$2" "$D/args"; then echo "  BAD $1"; fails=$((fails+1)); else echo "  ok  $1"; fi; }

echo "A1 — a CREATE succeeds first time and must not retry"
arm 201 999
out=$(seed_put "/p" -H 'Content-Type: x' --data-binary @/dev/null 2>/dev/null)
chk   "returns 201"              "$out"              "201"
chk   "exactly one request"      "$(cat "$D/calls")" "1"
hasnt "sent NO If-Match"         "If-Match"

echo "A2 — 428 retries once under If-Match: * and returns the RETRY's code"
arm 428 200
out=$(seed_put "/p" -H 'Content-Type: x' --data-binary @/dev/null 2>/dev/null)
chk "returns the retry's code"   "$out"              "200"
chk "exactly two requests"       "$(cat "$D/calls")" "2"
has "retry carried If-Match: *"  "If-Match: *"
has "retry forwarded body args"  "--data-binary @/dev/null"

# THE CONTROL ARMS. A2 alone passes for a helper that retries on
# EVERYTHING and for one that always reports 428; both are worse than
# no helper, and only these two legs tell them apart.
echo "A3 — CONTROL: a non-428 failure is NOT retried and NOT masked"
arm 503 200
out=$(seed_put "/p" -H 'Content-Type: x' 2>/dev/null)
chk "returned as-is"             "$out"              "503"
chk "exactly one request"        "$(cat "$D/calls")" "1"

echo "A4 — CONTROL: a 428 that stays 428 is reported, not swallowed"
arm 428 428
out=$(seed_put "/p" -H 'Content-Type: x' 2>/dev/null)
chk "surfaces the second 428"    "$out"              "428"
chk "stops after two"            "$(cat "$D/calls")" "2"

echo
if [ "$fails" = 0 ]; then echo "ALL GREEN"; else echo "$fails FAILED"; fi
exit "$fails"
