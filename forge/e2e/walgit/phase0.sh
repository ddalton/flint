#!/usr/bin/env bash
# PHASE 0 of the walgit control arm (docs/plans/flint-forge-simplification-2026-09-05.md §9).
#
# No cloud. walgit's own image, built from its Containerfile at a pinned
# commit, runs as a container beside a MinIO container on one Docker
# network; stock git on this machine pushes, clones, races, force-pushes,
# clones from a bundle, cold-starts it and cuts it off from its bucket.
# Everything it establishes is about walgit's MATURITY on the shape the
# scale rig will drive — not a leg of the comparison. Numbers here are
# loopback numbers and are not comparable to anything on EC2.
#
# Legs (each PASS / FAIL / INCONCLUSIVE; an INCONCLUSIVE is not a PASS):
#   W1  push to a repository that does not exist yet (auto-create), told ok
#   W2  clone it back; fsck clean; tip matches
#   W3  a second push is visible to the next fetch (push acknowledged ⇒ next read sees it)
#   W4  two pushes to one ref from the same base, concurrently: exactly one winner (falsifier 2's shape)
#   W5  a stale old-oid (--force-with-lease against a moved ref) is refused by the SERVER
#   W6  provenance: `wal ls` lists the entries; after a force-push, `wal materialize --at-seq` recovers the previous tip
#   W7  bundles: `bundle run` cuts the weekly full; a clone with transfer.bundleURI=true fetches it
#   W8  cold start: a fresh container with an empty cache — time to ready, to ls-remote, to a clone that matches
#   W9  the bucket cut off: reads and pushes while walgit cannot reach MinIO, and recovery when it can
#
# Knobs: WALGIT_IMAGE MINIO_PORT WALGIT_PORT NET BUCKET WORK KEEP PACK_MB
set -uo pipefail

WALGIT_IMAGE=${WALGIT_IMAGE:-walgit:e5295e6}
MINIO_PORT=${MINIO_PORT:-9101}
WALGIT_PORT=${WALGIT_PORT:-8090}
NET=${NET:-walgit-p0}
BUCKET=${BUCKET:-walgit}
PACK_MB=${PACK_MB:-8}
WORK=${WORK:-$(mktemp -d /tmp/walgit-p0.XXXXXX)}
KEEP=${KEEP:-no}
MINIO_NAME=walgit-p0-minio
SRV=walgit-p0
TOK=${WALGIT_TOKEN_AGENT:-$(openssl rand -hex 24)}
URL="http://127.0.0.1:${WALGIT_PORT}/acme/proj.git"
AUTH="http.extraHeader=Authorization: Bearer ${TOK}"
export AWS_ACCESS_KEY_ID=minioadmin AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1 AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true
export AWS_ENDPOINT_URL="http://127.0.0.1:${MINIO_PORT}"
export GIT_TERMINAL_PROMPT=0

PASS=0; FAIL=0; INCONC=0
say()    { printf '%s\n' "$*"; }
head_()  { printf '\n== %s ==\n' "$*"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
now()    { python3 -c 'import time; print(f"{time.time():.3f}")'; }
since()  { python3 -c "import time; print(f'{time.time()-$1:.2f}')"; }
g()      { git -c "$AUTH" "$@"; }            # authenticated git
wg()     { docker exec "$SRV" walgit "$@"; }  # walgit's CLI inside its own container, same config and bucket
api()    { curl -s -o "$WORK/api.out" -w '%{http_code}' -H "Authorization: Bearer $TOK" "$@"; }

cleanup() {
  if [ "$KEEP" = yes ]; then say "KEEP=yes: containers $SRV $MINIO_NAME, network $NET, work $WORK left in place"; return; fi
  docker rm -f "$SRV" "$MINIO_NAME" >/dev/null 2>&1
  docker network rm "$NET" >/dev/null 2>&1
  rm -rf "$WORK"
}
trap cleanup EXIT

# ── 0. preconditions ────────────────────────────────────────────────
head_ "preconditions"
DV=$(docker version --format '{{.Server.Version}}' 2>/dev/null) || { bad "docker is not answering"; exit 1; }
say "  docker $DV; image $WALGIT_IMAGE"
docker image inspect "$WALGIT_IMAGE" >/dev/null 2>&1 || { bad "image $WALGIT_IMAGE is not built"; exit 1; }
GV=$(git --version | awk '{print $3}')
case "$GV" in 2.4[6-9]*|2.[5-9]*|[3-9].*) ok "host git $GV (>= 2.46 for bundle-uri + credential authtype)";; *) bad "host git $GV < 2.46"; exit 1;; esac
say "  work: $WORK"

# ── 1. MinIO + bucket, walgit + config ──────────────────────────────
head_ "MinIO and walgit"
docker rm -f "$SRV" "$MINIO_NAME" >/dev/null 2>&1; docker network rm "$NET" >/dev/null 2>&1
docker network create "$NET" >/dev/null || { bad "network"; exit 1; }
docker run -d --name "$MINIO_NAME" --network "$NET" -p "127.0.0.1:${MINIO_PORT}:9000" \
  -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
  quay.io/minio/minio:latest server /data >/dev/null || { bad "minio did not start"; exit 1; }
for i in $(seq 1 30); do curl -sf "http://127.0.0.1:${MINIO_PORT}/minio/health/live" >/dev/null && break; sleep 1; done
aws s3api create-bucket --bucket "$BUCKET" >/dev/null 2>&1 && ok "minio up on :$MINIO_PORT, bucket $BUCKET" || { bad "bucket"; exit 1; }

cat > "$WORK/walgit.toml" <<EOF
[server]
listen = "0.0.0.0:8080"
public_url = "http://127.0.0.1:${WALGIT_PORT}"
auto_create_on_push = true
roles = []
[server.tls]
mode = "off"
[server.auth]
mode = "token"
anonymous_read = false
tokens = [ { principal = "agent", token_env = "WALGIT_TOKEN_AGENT", write = true, admin = true } ]
[store]
backend = "s3"
bucket = "${BUCKET}"
prefix = ""
[store.s3]
endpoint = "http://minio:9000"
region = "us-east-1"
access_key_env = "AWS_ACCESS_KEY_ID"
secret_key_env = "AWS_SECRET_ACCESS_KEY"
force_path_style = true
[cache]
dir = "/var/lib/walgit"
mode = "disk"
[maintenance]
interval = "15s"
disk = "ssd"
[[bundles.strategy]]
name = "weekly"
kind = "full"
schedule = "0 0 23 * * Sun"
keep = 1
backfill_max = 1
[[bundles.strategy]]
name = "daily"
kind = "incremental"
base = "weekly"
schedule = "0 0 23 * * *"
chain = true
[bundles]
require = []
[telemetry]
log_format = "pretty"
EOF

start_walgit() {  # start_walgit <name-suffix>: a fresh container, EMPTY cache (no volume) — the bucket is the repository
  docker rm -f "$SRV" >/dev/null 2>&1
  docker run -d --name "$SRV" --network "$NET" -p "127.0.0.1:${WALGIT_PORT}:8080" \
    -e WALGIT_TOKEN_AGENT="$TOK" -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
    -v "$WORK/walgit.toml:/etc/walgit/walgit.toml:ro" "$WALGIT_IMAGE" >/dev/null || return 1
  for i in $(seq 1 60); do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:${WALGIT_PORT}/readyz")" = 200 ] && return 0
    if [ "$(docker inspect -f '{{.State.Running}}' "$SRV" 2>/dev/null)" != true ]; then
      say "  walgit exited:"; docker logs "$SRV" 2>&1 | tail -15 | sed 's/^/    /'; return 1
    fi
    sleep 1
  done
  return 1
}
T0=$(now); start_walgit && ok "walgit ready in $(since "$T0") s (token auth, S3 backend on minio, cache mode disk)" \
  || { bad "walgit did not become ready"; docker logs "$SRV" 2>&1 | tail -20; exit 1; }
say "  $(wg --version 2>/dev/null | head -1)"

# ── W1: first push creates the repository ───────────────────────────
head_ "W1  first push, auto-create"
mkdir -p "$WORK/src" && cd "$WORK/src" && git init -q -b main . && git config user.email a@b && git config user.name agent
for i in 1 2 3; do echo "commit $i" > "f$i.txt"; git add "f$i.txt"; git commit -q -m "c$i"; done
head -c $((PACK_MB*1024*1024)) /dev/urandom > blob.bin && git add blob.bin && git commit -q -m "blob ${PACK_MB} MiB"
TIP=$(git rev-parse HEAD)
T0=$(now); OUT=$(g push "$URL" main 2>&1); RC=$?; DT=$(since "$T0")
if [ $RC -eq 0 ]; then ok "push of 4 commits + ${PACK_MB} MiB told ok in $DT s (repository created on push)"; else bad "push failed (rc=$RC): $(echo "$OUT" | tail -3)"; fi
[ "$(g ls-remote "$URL" refs/heads/main 2>/dev/null | cut -f1)" = "$TIP" ] && ok "ls-remote shows the pushed tip" || bad "ls-remote does not show the tip"

# ── W2: clone back ──────────────────────────────────────────────────
head_ "W2  clone"
cd "$WORK"; T0=$(now); g clone -q "$URL" c1 2>"$WORK/clone1.err"; RC=$?; DT=$(since "$T0")
if [ $RC -eq 0 ] && [ "$(git -C c1 rev-parse HEAD)" = "$TIP" ]; then
  git -C c1 fsck -q --strict 2>/dev/null && ok "clone in $DT s, tip matches, fsck --strict clean" || bad "clone fsck failed"
else bad "clone failed or tip differs: $(tail -2 "$WORK/clone1.err")"; fi

# ── W3: a push is visible to the next fetch ─────────────────────────
head_ "W3  second push visible to the next read"
cd "$WORK/src" && echo more > f4.txt && git add f4.txt && git commit -q -m c4 && TIP2=$(git rev-parse HEAD)
g push -q "$URL" main 2>/dev/null && git -C "$WORK/c1" -c "$AUTH" fetch -q origin 2>/dev/null
[ "$(git -C "$WORK/c1" rev-parse origin/main)" = "$TIP2" ] && ok "the fetch right after the push sees it" || bad "the fetch does not see the push"

# ── W4: two pushes to one ref from one base, concurrently ───────────
head_ "W4  concurrent pushes to one ref: exactly one winner"
cd "$WORK"; g clone -q "$URL" a 2>/dev/null; g clone -q "$URL" b 2>/dev/null
for c in a b; do git -C $c config user.email $c@b; git -C $c config user.name $c; echo $c > $c/$c.txt; git -C $c add $c.txt; git -C $c commit -q -m "from $c"; done
BASE=$TIP2
git -C a -c "$AUTH" push --force-with-lease=refs/heads/main:$BASE origin main >a.out 2>&1 & PA=$!
git -C b -c "$AUTH" push --force-with-lease=refs/heads/main:$BASE origin main >b.out 2>&1 & PB=$!
wait $PA; RA=$?; wait $PB; RB=$?
WON=$(( (RA==0) + (RB==0) ))
TIPNOW=$(g ls-remote "$URL" refs/heads/main | cut -f1)
if [ $WON -eq 1 ]; then
  W=a; [ $RB -eq 0 ] && W=b
  [ "$TIPNOW" = "$(git -C $W rev-parse HEAD)" ] && ok "exactly one winner ($W); main is the winner's tip; loser: $(grep -o 'rejected.*\|stale.*\|ng.*' ${W/a/b}.out ${W/b/a}.out 2>/dev/null | head -1)" \
    || bad "one winner but main is not its tip"
else bad "$WON winners (rc a=$RA b=$RB): $(tail -1 a.out) / $(tail -1 b.out)"; fi

# ── W5: a stale old-oid refused by the server ───────────────────────
head_ "W5  stale old-oid refused server-side"
L=a; [ "$TIPNOW" = "$(git -C a rev-parse HEAD)" ] && L=b   # the loser still holds the old base
OUT=$(git -C $L -c "$AUTH" push --force-with-lease=refs/heads/main:$BASE origin main 2>&1); RC=$?
if [ $RC -ne 0 ] && [ "$(g ls-remote "$URL" refs/heads/main | cut -f1)" = "$TIPNOW" ]; then ok "refused, main unchanged: $(echo "$OUT" | grep -i 'reject\|stale\|lease' | head -1)"; else bad "a stale push was accepted (rc=$RC)"; fi

# ── W6: provenance and undo ─────────────────────────────────────────
head_ "W6  wal ls, force-push, materialize --at-seq"
LS=$(wg wal ls acme/proj 2>&1); RC=$?
N=$(echo "$LS" | grep -c -i 'push\|ref_update\|seq' )
if [ $RC -eq 0 ] && [ "$N" -ge 3 ]; then ok "wal ls lists $N entries"; else inconc "wal ls: rc=$RC $(echo "$LS" | tail -2)"; fi
SEQ_BEFORE=$(echo "$LS" | grep -o -E '(^|[^0-9])[0-9]+' | tr -d ' ' | sort -n | tail -1)
PRE=$TIPNOW
g push -q --force "$URL" "$TIP2:refs/heads/main" 2>/dev/null && [ "$(g ls-remote "$URL" refs/heads/main | cut -f1)" = "$TIP2" ] && say "  force-pushed main back to ${TIP2:0:8} (was ${PRE:0:8})"
docker exec "$SRV" rm -rf /tmp/mat >/dev/null 2>&1
MAT=$(wg wal materialize acme/proj --at-seq "$SEQ_BEFORE" --out /tmp/mat 2>&1); RC=$?
GOT=$(docker exec "$SRV" git -C /tmp/mat rev-parse refs/heads/main 2>/dev/null || docker exec "$SRV" git -C /tmp/mat rev-parse main 2>/dev/null)
if [ $RC -eq 0 ] && [ "$GOT" = "$PRE" ]; then ok "materialize --at-seq $SEQ_BEFORE recovers the pre-force tip ${PRE:0:8} (forge has no counterpart: X15)"
elif [ $RC -eq 0 ]; then inconc "materialize ran but main there is ${GOT:0:8}, expected ${PRE:0:8} (seq guessed as $SEQ_BEFORE from: $(echo "$LS" | tail -3 | tr '\n' ' '))"
else inconc "materialize failed: $(echo "$MAT" | tail -2)"; fi
g push -q --force "$URL" "$PRE:refs/heads/main" 2>/dev/null   # put it back

# ── W7: bundles ─────────────────────────────────────────────────────
head_ "W7  weekly bundle cut, clone through bundle-uri"
BR=$(wg bundle run --repo acme/proj 2>&1); RC=$?
LIST=$(api "http://127.0.0.1:${WALGIT_PORT}/acme/proj.git/bundles/list"); BODY=$(cat "$WORK/api.out")
if [ "$LIST" = 200 ] && echo "$BODY" | grep -q -i 'uri'; then ok "bundles/list after 'bundle run': $(echo "$BODY" | grep -c -i 'uri') bundle(s) listed"
else inconc "bundles/list: HTTP $LIST; bundle run rc=$RC: $(echo "$BR" | tail -2)"; say "$(wg bundle plan acme/proj 2>&1 | tail -6 | sed 's/^/    /')"; fi
cd "$WORK"; rm -rf c2
GIT_TRACE_CURL="$WORK/curl.trace" g -c transfer.bundleURI=true clone -q "$URL" c2 2>"$WORK/clone2.err"; RC=$?
BUNDLE_REQ=$(grep -c -i 'GET /acme/proj.git/bundles/' "$WORK/curl.trace" 2>/dev/null || echo 0)
if [ $RC -eq 0 ] && [ "$(git -C c2 rev-parse HEAD)" = "$PRE" ] && [ "$BUNDLE_REQ" -ge 2 ]; then ok "clone with transfer.bundleURI=true fetched the list and a bundle ($BUNDLE_REQ bundle requests), tip matches"
elif [ $RC -eq 0 ]; then inconc "clone ok but $BUNDLE_REQ bundle requests seen (list + bundle expected)"
else bad "bundle-uri clone failed: $(tail -2 "$WORK/clone2.err")"; fi

# ── W8: cold start ──────────────────────────────────────────────────
head_ "W8  cold start from the bucket alone (fresh container, empty cache)"
T0=$(now); start_walgit || { bad "cold start: not ready"; exit 1; }; TR=$(since "$T0")
T0=$(now); LSR=$(g ls-remote "$URL" refs/heads/main 2>/dev/null | cut -f1); TL=$(since "$T0")
cd "$WORK"; rm -rf c3; T0=$(now); g clone -q "$URL" c3 2>/dev/null; RC=$?; TC=$(since "$T0")
if [ "$LSR" = "$PRE" ] && [ $RC -eq 0 ] && [ "$(git -C c3 rev-parse HEAD)" = "$PRE" ]; then
  ok "ready $TR s after start; first ls-remote $TL s and correct; first clone $TC s and correct (X14's number, loopback)"
else bad "cold start served the wrong state: ls-remote ${LSR:0:8}, clone rc=$RC"; fi

# ── W9: the bucket cut off ──────────────────────────────────────────
head_ "W9  walgit cut off from its bucket"
docker network disconnect "$NET" "$MINIO_NAME" >/dev/null 2>&1 || { inconc "could not disconnect minio"; }
sleep 1
T0=$(now); RD=$(g ls-remote "$URL" refs/heads/main 2>&1); RRC=$?; TD=$(since "$T0")
cd "$WORK/src" && echo out > f5.txt && git add f5.txt && git commit -q -m c5
PO=$(timeout 60 git -c "$AUTH" push "$URL" main 2>&1); PRC=$?
say "  read during the outage: rc=$RRC in $TD s: $(echo "$RD" | tail -1 | cut -c1-120)"
say "  push during the outage: rc=$PRC: $(echo "$PO" | grep -v '^$' | tail -1 | cut -c1-120)"
if [ $PRC -ne 0 ]; then ok "a push while the bucket is unreachable is refused"; else bad "a push was acknowledged with no bucket behind it"; fi
if [ $RRC -ne 0 ]; then ok "a read while the bucket is unreachable is refused (every read revalidates — the behaviour forge lacks, X13)"; else inconc "a read was served during the outage (freshness_ttl or a cached advertisement?): ${RD:0:8}"; fi
docker network connect "$NET" "$MINIO_NAME" >/dev/null 2>&1; sleep 2
g push -q "$URL" main 2>/dev/null && [ "$(g ls-remote "$URL" refs/heads/main 2>/dev/null | cut -f1)" = "$(git rev-parse HEAD)" ] && ok "recovers once the bucket returns: the push lands" || bad "did not recover after reconnect"

# ── verdict ─────────────────────────────────────────────────────────
head_ "verdict"
say "walgit $(wg --version 2>/dev/null | head -1) · image $WALGIT_IMAGE · $(date -u +%Y-%m-%dT%H:%M:%SZ)"
say "PASS $PASS  FAIL $FAIL  INCONCLUSIVE $INCONC"
[ $FAIL -eq 0 ] && [ $INCONC -eq 0 ] && exit 0
[ $FAIL -eq 0 ] && exit 2
exit 1
