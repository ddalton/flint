#!/usr/bin/env bash
# GIT QUALIFICATION — the reference client against forge, over real HTTP.
#
# There is no published conformance suite for a git SERVER. What exists
# is the reference implementation, and the qualification that matters is
# whether stock `git` — every version of it a user will point at this —
# can do the things the protocol says a server can do. So this drives
# the real client through the operations a server must support, against
# the real front (`flint-forge-gitcgi` + `git http-backend`), the real
# hook, the real syncer and a real S3 API.
#
# It is deliberately NOT the falsifier suite and NOT `f10-git-ops.sh`.
# f10 is everyday git in a cluster; this is the protocol's surface,
# including the corners forge has never been asked about: shallow and
# unshallow, partial clone, atomic push, force-with-lease, mirror,
# protocol v0 against v2, tags of every kind, and a fetch by object id.
#
# THREE OUTCOMES, and the difference between them is the point:
#   PASS      git could do it (or the policy refused it, where a
#             refusal is the documented answer)
#   FAIL      git could not do it and should have been able to
#   KNOWN     git could not do it, forge never claimed it could, and
#             the leg exists so the gap is written down rather than
#             discovered by a user
#
#   bash forge/e2e/gitqual/run-gitqual.sh
#   KEEP=1 bash forge/e2e/gitqual/run-gitqual.sh
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
MINIO_NAME=${MINIO_NAME:-flint-gitqual-minio}
MINIO_PORT=${MINIO_PORT:-9109}
BUCKET=${BUCKET:-gitqual}
export WORK=${WORK:-/tmp/fc-gitqual}
rm -rf "$WORK"; mkdir -p "$WORK"
# shellcheck source=../composition/rig.sh
. "$HERE/../composition/rig.sh"

GITCGI_BIN=${GITCGI_BIN:-$REPO_ROOT/forge/syncer/target/debug/flint-forge-gitcgi}
PORT=${PORT:-9723}
PFX=${PFX:-tenant/gitqual}
URL="http://127.0.0.1:$PORT/proj.git"
# The door sets this from a verified TokenReview; here the client sends
# it, which is exactly why the operator renders a NetworkPolicy in front
# of the port (design §6).
HDR="X-Remote-User: driller"

G() { git -c "http.extraHeader=$HDR" "$@"; }
GC() { local d=$1; shift; git -C "$d" -c "http.extraHeader=$HDR" \
         -c user.name=driller -c user.email=driller@invalid "$@"; }

trap 'kill "$(cat "$WORK/cgi.pid" 2>/dev/null)" 2>/dev/null; rig_clean' EXIT
rig_init || { echo "rig_init failed"; exit 1; }
[ -x "$GITCGI_BIN" ] || { echo "missing $GITCGI_BIN — cargo build --features gitcgi"; exit 1; }
binary_is_fresh || exit 1
rig_purge "$PFX/"

# ── the server: syncer behind the HTTP front ─────────────────────────
ROOT=$WORK/root; mkdir -p "$ROOT"
new_bare_repo "$ROOT/proj.git"
forge_up q "$ROOT/proj.git" "$PFX" \
  "FLINT_FORGE_ALLOW_NON_FF=refs/heads/force/*" \
  "FLINT_FORGE_FOLD_FACTOR=0" \
  "FLINT_FORGE_REPACK_THRESHOLD=100000"
wait_key "$PFX/git/epoch" 30 >/dev/null || { inconc "the syncer never claimed"; exit 2; }

( export GIT_PROJECT_ROOT="$ROOT" GIT_HTTP_EXPORT_ALL=1 \
         FLINT_FORGE_GIT_LISTEN="127.0.0.1:$PORT" \
         FLINT_FORGE_SOCKET=/tmp/fc-q.sock
  exec "$GITCGI_BIN" ) > "$WORK/cgi.log" 2>&1 &
echo $! > "$WORK/cgi.pid"
for _ in $(seq 1 40); do
  curl -fsS -o /dev/null "$URL/info/refs?service=git-upload-pack" 2>/dev/null && break
  sleep 0.25
done
curl -fsS -o /dev/null "$URL/info/refs?service=git-upload-pack" 2>/dev/null \
  || { inconc "the HTTP front never answered on $PORT"; exit 2; }

# The rig's own falsifiability leg: everything below goes over HTTP
# through the front, not over the file transport the other drills use.
head_ "precondition: the transport is HTTP through flint-forge-gitcgi"
if G ls-remote "$URL" >/dev/null 2>&1; then
  ok "ls-remote answers over http://127.0.0.1:$PORT"
else
  inconc "the front did not answer ls-remote"; exit 2
fi

check() {  # check <label> <ok|refuse> <shell>
  local label=$1 expect=$2 script=$3 out rc
  out=$(eval "$script" 2>&1); rc=$?
  case "$expect" in
    ok)     if [ $rc -eq 0 ]; then ok "$label"
            else bad "$label -> $(printf '%s' "$out" | tail -2 | tr '\n' ' ')"; fi ;;
    refuse) if [ $rc -ne 0 ]; then ok "$label (refused, as the policy says)"
            else bad "$label -> SUCCEEDED where a refusal was the documented answer"; fi ;;
  esac
}

# A leg whose failure is a GAP forge never claimed to fill. It is
# reported, named and counted apart, because a suite that fails on
# everything a server might one day support is a suite nobody reads.
gap() {  # gap <id> <label> <shell>
  local id=$1 label=$2 script=$3 out rc
  out=$(eval "$script" 2>&1); rc=$?
  if [ $rc -eq 0 ]; then ok "$label"
  else accepted "$id" "$label — $(printf '%s' "$out" | grep -iE 'error|fatal|warning' | head -1)"; fi
}

# ── the seed ─────────────────────────────────────────────────────────
head_ "discovery on an empty repository"
check "ls-remote on an empty repository is empty and succeeds" ok \
  '[ -z "$(G ls-remote "$URL")" ]'
check "clone of an empty repository succeeds" ok \
  'rm -rf "$WORK/empty" && G clone -q "$URL" "$WORK/empty" && [ -d "$WORK/empty/.git" ]'

WC=$WORK/wc
G clone -q "$URL" "$WC" 2>/dev/null
mkdir -p "$WC"
GC "$WC" init -q -b main 2>/dev/null
GC "$WC" remote add origin "$URL" 2>/dev/null
mkdir -p "$WC/dir"
printf 'one\n' > "$WC/a.txt"; printf 'deep\n' > "$WC/dir/b.txt"
GC "$WC" add -A; GC "$WC" commit -qm one
C1=$(GC "$WC" rev-parse HEAD)
check "push creates the default branch" ok 'GC "$WC" push -q origin HEAD:refs/heads/main'

head_ "advertisement"
check "ls-remote names the branch just pushed" ok \
  'G ls-remote "$URL" | grep -q "refs/heads/main"'
check "--symref advertises HEAD as a symbolic ref to the default branch" ok \
  'G ls-remote --symref "$URL" | grep -q "^ref: refs/heads/main[[:space:]]HEAD"'
check "protocol v0 and v2 advertise the same refs" ok \
  'diff <(git -c "http.extraHeader=$HDR" -c protocol.version=0 ls-remote "$URL") \
        <(git -c "http.extraHeader=$HDR" -c protocol.version=2 ls-remote "$URL") >/dev/null'

head_ "clone and fetch"
check "full clone reproduces the tree" ok \
  'rm -rf "$WORK/c1" && G clone -q "$URL" "$WORK/c1" && [ "$(cat "$WORK/c1/dir/b.txt")" = deep ]'
check "the clone passes fsck" ok \
  'git -C "$WORK/c1" fsck --no-progress >/dev/null'
check "clone over protocol v0" ok \
  'rm -rf "$WORK/c0" && git -c "http.extraHeader=$HDR" -c protocol.version=0 clone -q "$URL" "$WORK/c0" \
     && [ "$(git -C "$WORK/c0" rev-parse HEAD)" = "$C1" ]'
check "shallow clone --depth 1" ok \
  'rm -rf "$WORK/sh" && G clone -q --depth 1 "$URL" "$WORK/sh" && [ -f "$WORK/sh/.git/shallow" ]'
check "single-branch clone" ok \
  'rm -rf "$WORK/sb" && G clone -q --single-branch --branch main "$URL" "$WORK/sb"'
check "mirror clone, then fsck" ok \
  'rm -rf "$WORK/mir" && G clone -q --mirror "$URL" "$WORK/mir" && git -C "$WORK/mir" fsck --no-progress >/dev/null'

# More history, so deepen and prune have something to work with.
printf 'two\n' >> "$WC/a.txt"; GC "$WC" add -A; GC "$WC" commit -qm two
C2=$(GC "$WC" rev-parse HEAD)
GC "$WC" push -q origin HEAD:refs/heads/main
GC "$WC" push -q origin "HEAD:refs/heads/doomed"

check "fetch --unshallow completes the shallow clone" ok \
  'git -C "$WORK/sh" -c "http.extraHeader=$HDR" fetch -q --unshallow \
     && [ ! -f "$WORK/sh/.git/shallow" ] && git -C "$WORK/sh" cat-file -e '"$C1"'^{commit}'
check "fetch --prune drops a branch the server deleted" ok \
  'rm -rf "$WORK/pr" && G clone -q "$URL" "$WORK/pr" \
     && git -C "$WORK/pr" rev-parse --verify -q origin/doomed >/dev/null \
     && GC "$WC" push -q origin --delete refs/heads/doomed \
     && git -C "$WORK/pr" -c "http.extraHeader=$HDR" fetch -q --prune origin \
     && ! git -C "$WORK/pr" rev-parse --verify -q origin/doomed >/dev/null'
# NOT "the clone succeeded": a server that does not understand the
# filter serves a FULL clone and git only warns, so the leg has to
# assert that blobs are actually missing. The first draft asserted the
# clone worked and passed while the filter was being ignored.
gap "G1" "partial clone --filter=blob:none really omits the blobs" \
  'rm -rf "$WORK/pc" && G clone -q --filter=blob:none "$URL" "$WORK/pc" 2>/dev/null \
     && [ "$(git -C "$WORK/pc" rev-list --objects --all --missing=print 2>/dev/null | grep -c "^?")" -gt 0 ]'
# And the same trap the other way round: the fetch must leave the
# object behind, not merely exit 0.
gap "G2" "fetch by object id (uploadpack.allowAnySHA1InWant)" \
  'rm -rf "$WORK/bysha" && git init -q "$WORK/bysha" \
     && git -C "$WORK/bysha" -c "http.extraHeader=$HDR" fetch -q "$URL" '"$C1"' 2>/dev/null \
     && git -C "$WORK/bysha" cat-file -e '"$C1"'^{commit}'

head_ "tags"
GC "$WC" tag -a v1 -m "release one"
GC "$WC" tag light
check "push an annotated tag and a lightweight one" ok \
  'GC "$WC" push -q origin v1 light'
check "a fresh clone gets both tags, and the annotated one is a tag object" ok \
  'rm -rf "$WORK/tg" && G clone -q "$URL" "$WORK/tg" \
     && [ "$(git -C "$WORK/tg" cat-file -t v1)" = tag ] \
     && [ "$(git -C "$WORK/tg" cat-file -t light)" = commit ]'
check "push --delete removes a tag" ok \
  'GC "$WC" push -q origin --delete light && ! G ls-remote --tags "$URL" | grep -q "refs/tags/light$"'

head_ "push"
check "fast-forward" ok \
  'printf three >> "$WC/a.txt" && GC "$WC" add -A && GC "$WC" commit -qm three \
     && GC "$WC" push -q origin HEAD:refs/heads/main'
C3=$(GC "$WC" rev-parse HEAD)
check "a no-op push says everything is up to date" ok \
  'GC "$WC" push origin HEAD:refs/heads/main 2>&1 | grep -q "Everything up-to-date"'
check "non-fast-forward on a branch the policy does not open" refuse \
  'GC "$WC" push -q --force origin '"$C1"':refs/heads/main'
check "and the branch did not move" ok \
  '[ "$(G ls-remote "$URL" refs/heads/main | cut -f1)" = "'"$C3"'" ]'
check "force where the policy allows it" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/force/x \
     && GC "$WC" push -q --force origin '"$C1"':refs/heads/force/x \
     && [ "$(G ls-remote "$URL" refs/heads/force/x | cut -f1)" = "'"$C1"'" ]'
check "--force-with-lease against the ref the client last saw" ok \
  'GC "$WC" fetch -q origin && GC "$WC" push -q --force-with-lease=refs/heads/force/x:'"$C1"' \
     origin '"$C3"':refs/heads/force/x'
check "--force-with-lease with a stale expectation is refused" refuse \
  'GC "$WC" push -q --force-with-lease=refs/heads/force/x:'"$C1"' origin '"$C1"':refs/heads/force/x'
# The control first: without --atomic the SAME pair lands the good ref.
# Without it, "atomic-a is absent" could mean the pair was unpushable
# for some other reason and the atomic guarantee was never exercised.
check "the control: without --atomic the good half of the pair lands" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/atomic-ctl '"$C1"':refs/heads/main; \
   G ls-remote "$URL" refs/heads/atomic-ctl | grep -q .'
check "push --atomic: one bad ref and NEITHER lands" ok \
  'GC "$WC" push -q --atomic origin '"$C3"':refs/heads/atomic-a '"$C1"':refs/heads/main; \
   ! G ls-remote "$URL" refs/heads/atomic-a | grep -q .'
check "push -o reaches the server" ok \
  'GC "$WC" push -q -o strategy=ours origin '"$C3"':refs/heads/opt'
check "push --all" ok \
  'GC "$WC" branch -f allbranch '"$C3"' && GC "$WC" push -q --all origin \
     && G ls-remote "$URL" refs/heads/allbranch | grep -q .'
check "concurrent pushes to different branches both land" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/par1 & \
   GC "$WC" push -q origin '"$C2"':refs/heads/par2 & wait; \
   G ls-remote "$URL" refs/heads/par1 | grep -q . && G ls-remote "$URL" refs/heads/par2 | grep -q .'

head_ "names the protocol allows"
check "a ref with slashes" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/team/sub/feature'
check "a long ref name" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/'"$(printf 'x%.0s' $(seq 1 180))"''
check "a UTF-8 ref name" ok \
  'GC "$WC" push -q origin '"$C3"':refs/heads/fonctionnalité-日本語'

head_ "the repository the server hands back is whole"
check "a final clone matches the working tree's tip and passes fsck --strict" ok \
  'rm -rf "$WORK/fin" && G clone -q "$URL" "$WORK/fin" \
     && [ "$(git -C "$WORK/fin" rev-parse HEAD)" = "'"$C3"'" ] \
     && git -C "$WORK/fin" fsck --strict --no-progress >/dev/null'
check "and the bucket alone rebuilds the same tip" ok \
  'rm -rf "$WORK/frombucket" && mkdir -p "$WORK/frombucket" \
     && aws s3 cp "s3://'"$BUCKET"'/'"$PFX"'/git/snapshot" "$WORK/snap.json" >/dev/null \
     && python3 -c "import json;print(json.load(open(\"$WORK/snap.json\"))[\"refs\"][\"refs/heads/main\"])" \
        | grep -q "'"$C3"'"'

# ── a second implementation ──────────────────────────────────────────
# Stock git proves forge speaks what git speaks. It cannot prove forge
# speaks the PROTOCOL rather than git's habits, because a server that
# depended on the exact order the reference client sends things would
# pass every leg above. go-git is an independent implementation.
#
# When the toolchain or the module cache is not here the leg does not
# run, and it says so rather than counting: a leg that could not run is
# not a leg that passed, and it is also not a measurement that came out
# ambiguous.
head_ "a second implementation: go-git"
GOGIT_DIR=$HERE/gogit
if ! command -v go >/dev/null 2>&1; then
  note "go is not installed — the second-implementation leg did NOT run"
elif ! ( cd "$GOGIT_DIR" && GOFLAGS=-mod=mod go build -o "$WORK/gogitcheck" . ) >"$WORK/gogit-build.log" 2>&1; then
  note "go-git did not build (no module cache or no network?) — the leg did NOT run:
        $(tail -2 "$WORK/gogit-build.log")"
else
  rm -rf "$WORK/gogit-wc"
  if "$WORK/gogitcheck" "$URL" driller "$WORK/gogit-wc" > "$WORK/gogit.log" 2>&1; then
    sed 's/^/  /' "$WORK/gogit.log" | sed 's/^  *//' | while read -r l; do printf '%s\n' "  $l"; done
    PASS=$((PASS + $(grep -c PASS "$WORK/gogit.log")))
    # And the reference client must see what the second one wrote.
    check "stock git reads go-git's branch back" ok \
      'G ls-remote "$URL" refs/heads/gogit | grep -q .'
  else
    bad "go-git: $(tail -2 "$WORK/gogit.log" | tr '\n' ' ')"
  fi
fi

verdict "gitqual"
