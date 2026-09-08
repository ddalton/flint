#!/usr/bin/env bash
# F13 — ONE door, two clients, on real infrastructure.
#
# F12 drilled the syncer's side of the file API: its HTTP client wore
# the door's label and dialled the syncer's port directly, because the
# gateway did not serve the file API yet. Its own P6 says so, and arms
# the wake by hand "as the door would". This drill is the other half —
# the routing, the identity and the wake, through the gateway.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/f13-onedoor.sh
#
# THE CLAIM UNDER TEST. A git client and a browser backend reach ONE
# repository at ONE host and port, differing only in their path prefix,
# under one `spec.consumers` list and one wake.
#
# WHAT MAKES A GREEN RUN MEAN SOMETHING. Eight ways it could be vacuous,
# each with the leg that closes it:
#
#  1. Nothing is authenticated. P0 sends no credential, a garbage
#     credential and a non-consumer's real credential, and requires
#     401/401/403 — at BOTH doors, since a file door that admits whom
#     the git door refuses is a way around the git door.
#  2. The door is not actually in the path — the client reached the
#     syncer directly. P0 requires that an ordinary pod CANNOT reach
#     either syncer port, so every later 200 came through the gateway.
#  3. "One door" is asserted about two addresses. P1 builds both URLs
#     from ONE `$DOOR` string and asserts the authority component is
#     byte-identical.
#  4. The two clients never see each other's writes, so "one
#     repository" is two repositories that both answered. P2 reads each
#     door's write through the OTHER door, by CONTENT.
#  5. The principal is whatever the caller says. P3 sends a FORGED
#     X-Remote-User naming a different ServiceAccount and requires the
#     commit author to be the pod's real identity. F12 could not test
#     this: it set that header itself, standing in for the door.
#  6. The conflict check cannot fail. P4 sends the same If-Match from
#     concurrent writers and requires exactly one 200.
#  7. The repository never actually slept, so the wake proved nothing.
#     P6 requires replicas 0 AND a changed pod UID, and — the new part
#     — wakes it with a plain HTTP READ and nothing else. No annotation
#     is set by this script.
#  8. The 501 is really a 503 nobody would stop retrying. P7 asserts the
#     status AND the absence of Retry-After.
#
# P8 measures rather than asserts: what 200 small browser saves cost in
# S3. It cannot fail the drill; it prints a number, because nobody has
# one for this access pattern and forge's byte economics were measured
# on agent pushes.
set -uo pipefail
NS=${NS:-agents}
NS_SYS=${NS_SYS:-forge-system}
# ONE address. Every client below builds its URL from this and nothing
# else — which is what leg P1 checks.
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
TAG=${TAG:-drill-3rd-door}
: "${BUCKET:?}"; : "${PREFIX:?}"
REPO=${REPO:-f13}
REPO_OFF=${REPO_OFF:-f13off}
SAVES=${SAVES:-200}
REAP_WAIT=${REAP_WAIT:-420}
WORK=${WORK:-$(mktemp -d)}
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));     printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));     printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

APP=f13-app; AGENT=f13-agent; STRANGER=f13-stranger

# The browser backend's hop. It presents its OWN projected token — the
# door turns it into a principal — and NOTHING else. In particular it
# does not set X-Remote-User: P3 shows that when it tries, the door
# overrides it.
FILES="/repo/$NS/$REPO/files"
app() { # <method> <path-after-/files> [curl args...]
  local m=$1 p=$2; shift 2
  K exec -n "$NS" "$APP" -- sh -c "
    curl -sS -o /tmp/body -w '%{http_code}' -X $m \
      '$DOOR$FILES$p' \
      -H \"Authorization: Bearer \$(cat /var/run/secrets/forge/token)\" $* " 2>/dev/null
}
appbody() { K exec -n "$NS" "$APP" -- cat /tmp/body 2>/dev/null; }
apphdr()  { K exec -n "$NS" "$APP" -- cat /tmp/hdr 2>/dev/null; }

echo "== P0: the door is the only way in, and it refuses whom it should =="
sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" forge/e2e/f13-repo.yaml.tpl | K apply -f - >/dev/null
# The control repository for P7: everything the same, file API OFF.
sed -e "s|__REPO__|$REPO_OFF|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" forge/e2e/f13-repo.yaml.tpl \
  | sed -e 's|^    enabled: true|    enabled: false|' | K apply -f - >/dev/null

K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata: { name: $APP, namespace: $NS, labels: { role: browser-backend } }
spec:
  serviceAccountName: agent-runner
  restartPolicy: Never
  containers:
    - name: c
      image: curlimages/curl:8.10.1
      command: ["sleep","36000"]
      volumeMounts: [{ name: t, mountPath: /var/run/secrets/forge, readOnly: true }]
  volumes:
    - name: t
      projected:
        sources: [{ serviceAccountToken: { path: token, audience: forge.chert.us, expirationSeconds: 3600 } }]
---
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
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: f13-outsider, namespace: $NS }
---
apiVersion: v1
kind: Pod
metadata: { name: $STRANGER, namespace: $NS }
spec:
  serviceAccountName: f13-outsider
  restartPolicy: Never
  containers:
    - name: c
      image: curlimages/curl:8.10.1
      command: ["sleep","36000"]
      volumeMounts: [{ name: t, mountPath: /var/run/secrets/forge, readOnly: true }]
  volumes:
    - name: t
      projected:
        sources: [{ serviceAccountToken: { path: token, audience: forge.chert.us, expirationSeconds: 3600 } }]
EOF
K wait -n "$NS" --for=condition=Ready pod/$APP pod/$AGENT pod/$STRANGER --timeout=240s >/dev/null 2>&1

for i in $(seq 1 60); do
  ph=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.phase}' 2>/dev/null)
  [ "$ph" = "Ready" ] && break
  sleep 5
done
[ "${ph:-}" = "Ready" ] && ok "the repository is Ready" \
  || { bad "the repository never became Ready (phase=${ph:-none}); the rest measures nothing"; exit 1; }

# The door must be serving the NEW route table. A door on the old image
# 404s /repo/... and every leg below would read as a routing failure.
code=$(app GET "?path=/")
if [ "$code" = "404" ]; then
  bad "the door 404s /repo/... — it is not running an image with the third door"
  echo "F13 cannot proceed."; exit 1
fi
ok "the door serves /repo/<ns>/<name>/files (got $code)"

# No credential at all.
code=$(K exec -n "$NS" "$APP" -- curl -sS -o /dev/null -D /tmp/hdr -w '%{http_code}' \
        "$DOOR$FILES?path=/" 2>/dev/null)
[ "$code" = "401" ] && ok "no credential: 401" || bad "no credential answered $code"
# …and NOT a Basic challenge. The git door must send one; this one must
# not, or a stray browser gets a native password dialog.
if K exec -n "$NS" "$APP" -- grep -qi '^www-authenticate' /tmp/hdr 2>/dev/null; then
  bad "the file door sent a Basic challenge"
else
  ok "and no WWW-Authenticate challenge, unlike the git door"
fi
# A garbage credential.
code=$(K exec -n "$NS" "$APP" -- curl -sS -o /dev/null -w '%{http_code}' \
        "$DOOR$FILES?path=/" -H "Authorization: Bearer not-a-token" 2>/dev/null)
[ "$code" = "401" ] && ok "a garbage credential: 401" || bad "garbage answered $code"

# A REAL token whose ServiceAccount is not in spec.consumers — 403 at
# BOTH doors, or the file door is a way around the git door.
code=$(K exec -n "$NS" "$STRANGER" -- sh -c "curl -sS -o /dev/null -w '%{http_code}' \
  '$DOOR$FILES?path=/' -H \"Authorization: Bearer \$(cat /var/run/secrets/forge/token)\"" 2>/dev/null)
[ "$code" = "403" ] && ok "a non-consumer at the FILE door: 403" || bad "non-consumer answered $code"
code=$(K exec -n "$NS" "$STRANGER" -- sh -c "curl -sS -o /dev/null -w '%{http_code}' \
  '$DOOR/git/$NS/$REPO.git/info/refs?service=git-upload-pack' \
  -u \"x:\$(cat /var/run/secrets/forge/token)\"" 2>/dev/null)
[ "$code" = "403" ] && ok "the same non-consumer at the GIT door: 403" \
  || bad "the two doors disagree about a non-consumer: git answered $code"

# THE CONTROL THAT SAYS THE DOOR IS IN THE PATH. An ordinary pod must
# not be able to reach the syncer's ports at all; if it can, every 200
# above might have bypassed the gateway.
EP=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.apiEndpoint}')
direct=$(K exec -n "$NS" "$APP" -- curl -sS -o /dev/null -w '%{http_code}' \
          --max-time 8 "$EP/files?path=/" 2>/dev/null || echo "blocked")
case "$direct" in
  000|blocked|"") ok "an ordinary pod cannot reach the syncer's file port directly" ;;
  *) bad "the syncer's file port answered an ordinary pod ($direct) — the NetworkPolicy is not enforcing, so every leg below may have bypassed the door" ;;
esac

echo "== P1: one address, two prefixes =="
AUTH_G=$(printf '%s' "$DOOR/git/$NS/$REPO.git/info/refs" | sed -E 's|^[a-z]+://([^/]+).*|\1|')
AUTH_F=$(printf '%s' "$DOOR$FILES" | sed -E 's|^[a-z]+://([^/]+).*|\1|')
[ "$AUTH_G" = "$AUTH_F" ] && ok "both doors are the same authority: $AUTH_G" \
  || bad "two addresses, not one: git=$AUTH_G files=$AUTH_F"
K exec -n "$NS" "$AGENT" -- sh -c "
  git config --global credential.helper '!f(){ echo username=x; echo password=\$(cat /var/run/secrets/forge/token); };f' &&
  git config --global user.email a@b.c && git config --global user.name agent &&
  rm -rf /tmp/c && git clone -q $DOOR/git/$NS/$REPO.git /tmp/c" >/dev/null 2>&1 \
  && ok "a real git clone through the door" || bad "the git clone failed"
code=$(app GET "?path=/")
[ "$code" = "200" ] && ok "and a REST list at the same authority" || bad "the REST list answered $code"

echo "== P2: each door reads the other's bytes =="
code=$(app PUT "/content?path=ui.txt" \
  "-H 'Content-Type: text/plain' --data-binary 'written-by-the-browser'")
case "$code" in
  200|201) ok "the browser wrote ui.txt ($code)" ;;
  *)       bad "the browser write answered $code: $(appbody)" ;;
esac
got=$(K exec -n "$NS" "$AGENT" -- sh -c "
  cd /tmp/c && git fetch -q origin && git show origin/agents:ui.txt 2>/dev/null" 2>/dev/null | tr -d '\r\n')
[ "$got" = "written-by-the-browser" ] && ok "git reads the browser's bytes: '$got'" \
  || bad "git saw '$got'"
K exec -n "$NS" "$AGENT" -- sh -c "
  cd /tmp/c && git checkout -q -B agents origin/agents &&
  printf 'written-by-the-agent' > agent.txt && git add agent.txt &&
  git commit -qm agent && git push -q origin agents" >/dev/null 2>&1 \
  && ok "the agent pushed through the door" || bad "the agent push failed"
code=$(app GET "/content?path=agent.txt"); got=$(appbody)
[ "$code" = "200" ] && [ "$got" = "written-by-the-agent" ] \
  && ok "the browser reads the agent's bytes: '$got'" || bad "the browser saw $code '$got'"

echo "== P3: the door decides who you are =="
# F12 could not run this leg: it SET this header itself. Here the door
# is in the path, so a forged one must be overridden.
code=$(app PUT "/content?path=forged.txt" \
  "-H 'Content-Type: text/plain' -H 'X-Remote-User: system:serviceaccount:kube-system:admin' --data-binary 'forged'")
if [ "$code" = "200" ] || [ "$code" = "201" ]; then
  author=$(K exec -n "$NS" "$AGENT" -- sh -c "
    cd /tmp/c && git fetch -q origin && git log -1 --format='%an <%ae>' origin/agents" 2>/dev/null | tr -d '\r')
  case "$author" in
    *kube-system*|*admin*) bad "the forged principal reached the commit: $author" ;;
    *agent-runner*)        ok "the door overrode the forged principal: $author" ;;
    *) inconc "the author is '$author' — neither the forgery nor the expected identity" ;;
  esac
else
  bad "the forged-header write answered $code, so nothing was measured"
fi

echo "== P4: two browser tabs, one file, no silent loss =="
etag=$(K exec -n "$NS" "$APP" -- sh -c "
  curl -sS -D /tmp/h -o /dev/null '$DOOR$FILES/content?path=ui.txt' \
    -H \"Authorization: Bearer \$(cat /var/run/secrets/forge/token)\" >/dev/null 2>&1;
  grep -i '^etag' /tmp/h | tr -d '\r\n' | sed 's/^[Ee][Tt][Aa][Gg]: *//'" 2>/dev/null)
if [ -z "$etag" ]; then
  inconc "no ETag came back, so the conditional write cannot be tested"
else
  note "both writers send If-Match: $etag"
  : > "$WORK/p4"
  for w in 1 2 3 4; do
    ( K exec -n "$NS" "$APP" -- sh -c "
        curl -sS -o /dev/null -w '%{http_code}\n' -X PUT '$DOOR$FILES/content?path=ui.txt' \
          -H \"Authorization: Bearer \$(cat /var/run/secrets/forge/token)\" \
          -H 'If-Match: $etag' -H 'Content-Type: text/plain' \
          --data-binary 'tab-$w'" 2>/dev/null >> "$WORK/p4" ) &
  done
  wait
  wins=$(grep -c '^20[01]$' "$WORK/p4" || true)
  lost=$(grep -cE '^(409|412)$' "$WORK/p4" || true)
  [ "$wins" = "1" ] && ok "exactly one writer won ($wins x 2xx, $lost x 409/412)" \
    || bad "$wins writers won — a lost update (codes: $(tr '\n' ' ' < "$WORK/p4"))"
fi

echo "== P5: the branch policy applies to a browser save =="
# `fileApi.branch` is a Deployment env var, so this ROLLS THE POD. A
# fixed sleep races it: a 403 from a pod still running the old branch
# would pass this leg for the wrong reason, and a request to a
# terminating pod would fail it for the wrong reason. Wait for the env
# to actually be what was asked for.
settle() { # <branch>
  local want=$1
  for _ in $(seq 1 60); do
    K rollout status -n "$NS" deploy -l chert.us/repo="$REPO" --timeout=10s >/dev/null 2>&1
    got=$(K get -n "$NS" pod -l chert.us/repo="$REPO" \
      -o jsonpath='{.items[0].spec.containers[*].env[?(@.name=="FLINT_FORGE_FILE_BRANCH")].value}' 2>/dev/null)
    [ "$got" = "$want" ] && { K wait -n "$NS" --for=condition=Ready pod -l chert.us/repo="$REPO" \
        --timeout=120s >/dev/null 2>&1; return 0; }
    sleep 5
  done
  return 1
}
K patch -n "$NS" flintrepo "$REPO" --type=merge \
  -p '{"spec":{"fileApi":{"enabled":true,"branch":"main","maxMb":2}}}' >/dev/null
if settle main; then
  code=$(app PUT "/content?path=protected.txt" "-H 'Content-Type: text/plain' --data-binary 'nope'")
  [ "$code" = "403" ] && ok "a save at a protected branch: 403" \
    || bad "a save at protected main answered $code: $(appbody)"
else
  inconc "the syncer never picked up branch=main, so the policy was never exercised"
fi
K patch -n "$NS" flintrepo "$REPO" --type=merge \
  -p '{"spec":{"fileApi":{"enabled":true,"branch":"agents","maxMb":2}}}' >/dev/null
settle agents || inconc "the syncer did not return to branch=agents; P6 may measure the wrong thing"

echo "== P6: the browser wakes a sleeping repository =="
K patch -n "$NS" flintrepo "$REPO" --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":60}}}' >/dev/null
before=$(K get -n "$NS" pod -l chert.us/repo="$REPO" -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null)
reaped=0
for i in $(seq 1 $((REAP_WAIT/10))); do
  r=$(K get -n "$NS" deploy -l chert.us/repo="$REPO" -o jsonpath='{.items[0].spec.replicas}' 2>/dev/null)
  n=$(K get -n "$NS" pod -l chert.us/repo="$REPO" --no-headers 2>/dev/null | wc -l | tr -d ' ')
  if [ "$r" = "0" ] && [ "$n" = "0" ]; then reaped=1; break; fi
  sleep 10
done
if [ "$reaped" != "1" ]; then
  bad "the repository never slept — P6 measures nothing without it"
else
  ok "the repository was reaped: replicas 0 and no pod"
  # NOTHING is annotated by this script. THE READ IS THE WAKE — the
  # capability that did not exist before the third door, and the reason
  # F12's own P6 had to arm the annotation by hand.
  K annotate -n "$NS" flintrepo "$REPO" chert.us/requested-at- >/dev/null 2>&1
  t0=$(date +%s)
  code=$(app GET "/content?path=agent.txt" "--max-time 300")
  t1=$(date +%s); got=$(appbody)
  after=$(K get -n "$NS" pod -l chert.us/repo="$REPO" -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null)
  [ "$code" = "200" ] && [ "$got" = "written-by-the-agent" ] \
    && ok "a plain HTTP READ woke it in $((t1-t0))s and the content survived" \
    || bad "the wake read answered $code '$got' after $((t1-t0))s"
  [ -n "$after" ] && [ "$after" != "$before" ] \
    && ok "and it is a FRESH pod ($before -> $after)" \
    || bad "the pod UID did not change — this was not a restore"
  armed=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.metadata.annotations.chert\.us/requested-at}' 2>/dev/null)
  [ -n "$armed" ] && ok "the DOOR armed the wake annotation: $armed" \
    || bad "nothing armed the annotation, so the pod came back for some other reason"
fi

echo "== P7: a repository without the file API says so, permanently =="
code=$(K exec -n "$NS" "$APP" -- sh -c "curl -sS -o /dev/null -D /tmp/hdr -w '%{http_code}' \
  '$DOOR/repo/$NS/$REPO_OFF/files?path=/' \
  -H \"Authorization: Bearer \$(cat /var/run/secrets/forge/token)\"" 2>/dev/null)
[ "$code" = "501" ] && ok "fileApi disabled: 501" || bad "fileApi disabled answered $code (503 would be retried forever)"
if K exec -n "$NS" "$APP" -- grep -qi '^retry-after' /tmp/hdr 2>/dev/null; then
  bad "the 501 carried Retry-After — it invites a retry that can never succeed"
else
  ok "and no Retry-After"
fi

echo "== P8: what $SAVES small browser saves cost in S3 (MEASURED, not asserted) =="
pfx="s3://$BUCKET/$PREFIX/$REPO/"
b0=$(aws s3 ls --recursive --summarize "$pfx" 2>/dev/null | awk '/Total Size/{print $3}')
o0=$(aws s3 ls --recursive --summarize "$pfx" 2>/dev/null | awk '/Total Objects/{print $3}')
s0=$(date +%s)
K exec -n "$NS" "$APP" -- sh -c "
  T=\$(cat /var/run/secrets/forge/token)
  i=0; while [ \$i -lt $SAVES ]; do
    curl -sS -o /dev/null -X PUT '$DOOR$FILES/content?path=notes.md' \
      -H \"Authorization: Bearer \$T\" -H 'Content-Type: text/plain' \
      --data-binary \"save number \$i, the kind a person makes while typing\"
    i=\$((i+1))
  done" >/dev/null 2>&1
s1=$(date +%s)
sleep 30
b1=$(aws s3 ls --recursive --summarize "$pfx" 2>/dev/null | awk '/Total Size/{print $3}')
o1=$(aws s3 ls --recursive --summarize "$pfx" 2>/dev/null | awk '/Total Objects/{print $3}')
if [ -n "${b0:-}" ] && [ -n "${b1:-}" ]; then
  note "saves: $SAVES in $((s1-s0))s"
  note "bytes resident: $b0 -> $b1 (delta $((b1-b0)); $(( (b1-b0) / (SAVES>0?SAVES:1) )) per save)"
  note "objects:        $o0 -> $o1 (delta $((o1-o0)))"
  note "THIS IS THE NUMBER TO COMPARE against an agent-push workload; forge's"
  note "byte economics were measured on pushes, never on a typing cadence."
else
  inconc "could not read the bucket, so the save cost was not measured"
fi

echo
echo "==== F13: $PASS passed, $FAIL failed, $INCONC inconclusive ===="
echo "work: $WORK"
[ "$FAIL" -eq 0 ] && [ "$INCONC" -eq 0 ]
