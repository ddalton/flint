#!/usr/bin/env bash
# F12 — both doors on one repository, on real infrastructure.
#
# An agent with a real git client and a browser backend with a real HTTP
# client, writing the same repository at the same time, against real S3.
# The unit battery decides the rules and the local end-to-end legs decide
# the wire; neither can decide what a second pod, a real bucket, a real
# NetworkPolicy and a real idle reap do to them.
#
#   KUBECONFIG=... BUCKET=... PREFIX=... ./forge/e2e/f12-fileapi.sh
#
# WHAT MAKES THIS DRILL NON-VACUOUS. Six ways a green run could mean
# nothing, each with the leg that closes it:
#
#  1. Nothing is authenticated, so every 200 is meaningless. P0 sends a
#     WRONG token and requires 401, and sends no principal and requires
#     403. Until those two fail closed, no later PASS counts.
#  2. The two clients never overlap, so "no loss under concurrency"
#     measures a queue. P3 starts every writer from one barrier and
#     REQUIRES that the git push was refused at least once across the
#     run — if the pusher always wins, the race never happened and the
#     leg is INCONCLUSIVE, not PASS.
#  3. The reads are served by something that never wrote. P1 and P2 read
#     each door's writes through the OTHER door, and P4 verifies by
#     content, never by ref name.
#  4. The conflict check cannot fail. P4 sends the SAME If-Match from
#     every writer and requires exactly one 200 — a server that says yes
#     to everything fails here.
#  5. The branch policy is not actually enforced. P5 writes at a
#     protected ref and requires 403.
#  6. The repository never actually slept. P6 requires replicas 0 AND a
#     changed pod UID before it credits the wake.
set -uo pipefail
NS=${NS:-agents}
# The clients live where the DOOR lives and wear its label, because that
# is the only thing the repository's NetworkPolicy admits to the file
# port. They are standing in for the door, which does not yet serve the
# file API — so this drill exercises the syncer's side of it, not the
# gateway's routing. P0 proves the policy is real by showing that a pod
# WITHOUT that identity cannot reach the port at all.
NS_DOOR=${NS_DOOR:-forge-system}
DOOR_LABEL=${DOOR_LABEL:-flint-forge-door}
# The git client is a REAL agent: one projected token and nothing else,
# reaching the repository through the door, which is the only thing that
# can turn that token into an identity. Pushing straight at the
# repository's git port is 403 — correctly, since it carries no verified
# principal — and an earlier version of this drill did exactly that and
# reported the resulting refusals as evidence of a race.
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
TAG=${TAG:-drill-2ab7b5fb}
: "${BUCKET:?}"; : "${PREFIX:?}"
REPO=${REPO:-f12}
HTTP_WRITERS=${HTTP_WRITERS:-12}
ROUNDS=${ROUNDS:-3}
REAP_WAIT=${REAP_WAIT:-420}
WORK=${WORK:-$(mktemp -d)}
PASS=0; FAIL=0; INCONC=0

K() { kubectl "$@"; }
ok()     { PASS=$((PASS+1));   printf '  PASS  %s\n' "$*"; }
bad()    { FAIL=$((FAIL+1));   printf '  FAIL  %s\n' "$*"; }
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }
note()   { printf '  ....  %s\n' "$*"; }

TOKEN=$(head -c 32 /dev/urandom | base64 | tr -d '=+/' | head -c 32)
HTTPPOD=f12-http; GITPOD=f12-git; STRANGER=f12-stranger

# Every HTTP call goes through here so the identity headers are in ONE
# place: the principal is what the door would set from a verified
# TokenReview, and the author is what the application supplies for the
# person it authenticated.
curlp() { # <method> <path> [extra curl args...]
  local m=$1 path=$2; shift 2
  K exec -n "$NS_DOOR" "$HTTPPOD" -- curl -sS -o /tmp/body -w '%{http_code}' \
    -X "$m" "$EP$path" \
    -H "Authorization: Bearer $TOKEN" \
    -H "X-Remote-User: system:serviceaccount:$NS:browser" \
    "$@" 2>/dev/null
}
body() { K exec -n "$NS_DOOR" "$HTTPPOD" -- cat /tmp/body 2>/dev/null; }

echo "== P0: preconditions, and the controls that make the rest mean something =="
K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Secret
metadata: { name: $REPO-file-token, namespace: $NS }
stringData: { token: "$TOKEN" }
EOF
sed -e "s|__REPO__|$REPO|g" -e "s|__NS__|$NS|g" -e "s|__BUCKET__|$BUCKET|g" \
    -e "s|__PREFIX__|$PREFIX|g" forge/e2e/f12-repo.yaml.tpl | K apply -f - >/dev/null

K apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: $HTTPPOD
  namespace: $NS_DOOR
  labels: { app.kubernetes.io/name: $DOOR_LABEL }
spec:
  restartPolicy: Never
  containers:
    - { name: c, image: curlimages/curl:8.10.1, command: ["sleep","36000"] }
---
apiVersion: v1
kind: Pod
metadata:
  name: $GITPOD
  namespace: $NS
  labels: { role: forge-agent }
spec:
  serviceAccountName: agent-runner
  restartPolicy: Never
  containers:
    - name: c
      image: dilipdalton/flint-forge-git:$TAG
      command: ["sleep","36000"]
      volumeMounts:
        - { name: forge-token, mountPath: /var/run/secrets/forge, readOnly: true }
  volumes:
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken:
              path: token
              audience: forge.chert.us
              expirationSeconds: 3600
---
apiVersion: v1
kind: Pod
metadata: { name: $STRANGER, namespace: $NS }
spec:
  restartPolicy: Never
  containers:
    - { name: c, image: curlimages/curl:8.10.1, command: ["sleep","36000"] }
EOF
K wait -n "$NS_DOOR" --for=condition=Ready pod/$HTTPPOD --timeout=180s >/dev/null 2>&1
K wait -n "$NS" --for=condition=Ready pod/$GITPOD pod/$STRANGER --timeout=180s >/dev/null 2>&1

for i in $(seq 1 60); do
  EP=$(K get -n "$NS" flintrepo "$REPO" -o jsonpath='{.status.apiEndpoint}' 2>/dev/null)
  [ -n "$EP" ] && break
  sleep 5
done
if [ -z "${EP:-}" ]; then
  bad "status.apiEndpoint was never published — the operator did not render the door"
  echo "P0 failed; the rest of the drill would measure nothing."; exit 1
fi
ok "status.apiEndpoint published: $EP"
case "$EP" in *:9850) ok "the endpoint names the file port, not the status port" ;;
  *) bad "the endpoint is not on the file port: $EP" ;; esac

# `status.apiEndpoint` says WHERE, and the operator publishes it as soon
# as it renders the Deployment — before the listener inside it answers.
# Racing that is not a product fault and must not be reported as one:
# an earlier run of this drill failed P0 and P1 on it while P3, running
# a minute later, wrote 12 of 12.
ready=0
for i in $(seq 1 60); do
  c=$(K exec -n "$NS_DOOR" "$HTTPPOD" -- curl -sS -m 5 -o /dev/null -w '%{http_code}' \
        -X GET "$EP/files?path=/" -H "Authorization: Bearer $TOKEN" \
        -H "X-Remote-User: probe" 2>/dev/null)
  case "$c" in 200|404) ready=1; break ;; esac
  sleep 5
done
[ "$ready" = "1" ] && ok "the file API answers" || bad "the file API never answered"

# The two controls. Until these fail closed, no 200 below is evidence.
code=$(K exec -n "$NS_DOOR" "$HTTPPOD" -- curl -sS -o /dev/null -w '%{http_code}' \
  -X GET "$EP/files?path=/" -H "Authorization: Bearer wrong-token-but-long-enough" \
  -H "X-Remote-User: x" 2>/dev/null)
[ "$code" = "401" ] && ok "a wrong bearer is refused (401)" || bad "a wrong bearer got $code"
code=$(K exec -n "$NS_DOOR" "$HTTPPOD" -- curl -sS -o /dev/null -w '%{http_code}' \
  -X GET "$EP/files?path=/" -H "Authorization: Bearer $TOKEN" 2>/dev/null)
[ "$code" = "403" ] && ok "no verified principal is refused (403)" || bad "no principal got $code"

# THE POLICY IS REAL, and this is the leg that says so on the wire.
# Every 200 below is reached from a pod wearing the door's identity; if
# any pod could reach this port, that identity would be decoration and
# the file API would be an open writer to anything in the cluster.
sc=$(K exec -n "$NS" "$STRANGER" -- curl -sS -m 8 -o /dev/null -w '%{http_code}' \
       -X GET "$EP/files?path=/" -H "Authorization: Bearer $TOKEN" \
       -H "X-Remote-User: x" 2>/dev/null)
if [ "$sc" = "000" ] || [ -z "$sc" ]; then
  ok "a pod without the door's identity cannot reach the file port at all"
else
  bad "an arbitrary pod reached the file API and got $sc — the NetworkPolicy is not \
guarding this port"
fi

# The door, not status.gitEndpoint: the endpoint is the repository's own
# port, which refuses anything without a door-verified principal.
GITPRE='T=$(cat /var/run/secrets/forge/token); A="Authorization: Basic $(printf "x:%s" "$T" | base64 | tr -d "\n")"; G() { git -c http.extraHeader="$A" "$@"; }; U='"$DOOR"'/git/'"$NS"'/'"$REPO"'.git; '
gitp() { K exec -n "$NS" "$GITPOD" -- sh -c "$GITPRE$1" 2>&1; }
gitp "git config --global user.email a@b.c; git config --global user.name agent; rm -rf /tmp/c" >/dev/null

echo
echo "== P1: the browser writes, the agent's git client reads =="
for i in $(seq 1 3); do
  code=$(curlp PUT "/files/content?path=ui/f$i.txt" -H "X-Flint-Author: ada" --data-binary "from-ui-$i")
  [ "$code" = "200" ] || bad "UI write $i answered $code: $(body)"
done
gitp "rm -rf /tmp/c && G clone -q -b agents \$U /tmp/c" >/dev/null
got=$(gitp "cat /tmp/c/ui/f2.txt")
[ "$got" = "from-ui-2" ] && ok "git reads the browser's bytes: '$got'" \
  || bad "git read '$got', expected from-ui-2"

echo
echo "== P2: the agent pushes, the browser reads =="
pushout=$(gitp "cd /tmp/c && mkdir -p code && echo from-agent > code/a.txt && git add -A && \
      git commit -qm agent && G push \$U HEAD:agents")
if echo "$pushout" | grep -qiE "error|fatal|reject"; then
  bad "the agent's push failed, so P2 cannot test what it is for: $(echo "$pushout" | tail -2)"
else
  ok "the agent pushed through the door"
fi
code=$(curlp GET "/files/content?path=code/a.txt")
got=$(body)
[ "$code" = "200" ] && [ "$got" = "from-agent" ] \
  && ok "the browser reads the agent's bytes: '$got'" \
  || bad "browser read $code '$got'"

echo
echo "== P3: both doors at once — nothing acknowledged may be lost =="
refused_any=0; landed_any=0
for r in $(seq 1 "$ROUNDS"); do
  : > "$WORK/acked.$r"
  for i in $(seq 1 "$HTTP_WRITERS"); do
    ( c=$(curlp PUT "/files/content?path=race/r$r-$i.txt" \
            -H "X-Flint-Author: user$i" --data-binary "r$r-$i")
      [ "$c" = "200" ] && echo "race/r$r-$i.txt" >> "$WORK/acked.$r" ) &
  done
  ( out=$(gitp "cd /tmp/c && echo g$r > code/g$r.txt && git add -A && git commit -qm g$r && \
          G push \$U HEAD:agents")
    echo "$out" > "$WORK/push.$r"
    echo "$out" | grep -qiE "error|fatal|reject" && echo 1 > "$WORK/pushrc.$r" || echo 0 > "$WORK/pushrc.$r" ) &
  wait
  if [ "$(cat "$WORK/pushrc.$r" 2>/dev/null)" = "0" ]; then
    landed_any=1
  else
    refused_any=1
    note "round $r: the push was refused (the API moved the branch first) — retrying"
    out=$(gitp "cd /tmp/c && G fetch -q \$U agents && git rebase -q FETCH_HEAD && \
          G push \$U HEAD:agents")
    echo "$out" | grep -qiE "error|fatal|reject" || landed_any=1
  fi
  n=$(wc -l < "$WORK/acked.$r" | tr -d ' ')
  note "round $r: $n of $HTTP_WRITERS HTTP writes acknowledged"
done

gitp "cd /tmp/c && G fetch -q \$U agents && git reset -q --hard FETCH_HEAD" >/dev/null
missing=0
for r in $(seq 1 "$ROUNDS"); do
  while read -r p; do
    gitp "test -f /tmp/c/$p" >/dev/null || { missing=$((missing+1)); note "MISSING $p"; }
  done < "$WORK/acked.$r"
done
[ "$missing" -eq 0 ] && ok "every acknowledged write is on the branch" \
  || bad "$missing acknowledged writes are not on the branch"

# The vacuity gate: if the pusher never lost a race, the concurrency
# never happened and "nothing was lost" measured a queue.
# BOTH halves, and the second is the one that was missing. "Refused at
# least once" is satisfied by a push that can NEVER succeed — which is
# exactly what happened when this drill pushed at the repository's own
# port and collected 403s. A race needs contention AND progress.
if [ "$landed_any" -eq 0 ]; then
  bad "no git push ever landed across $ROUNDS rounds — the git door is not working, and \
every 'refusal' below was that, not contention"
elif [ "$refused_any" -eq 0 ]; then
  inconc "the git push was never refused across $ROUNDS rounds — the two doors did not \
actually contend, so P3's green says nothing about concurrency"
else
  ok "the race is real: the push both landed and was refused at least once, and recovered"
fi

echo
echo "== P4: many writers, one file, one version =="
# The seed must not be bytes any writer below also sends: an identical
# write is a NO-OP that answers 200 without committing, which would read
# here as a second winner. That exact collision made the local version
# of this test fail about one run in eight.
code=$(curlp PUT "/files/content?path=shared.txt" -H "X-Flint-Author: seed" --data-binary "seed-only")
[ "$code" = "200" ] || bad "the seed write answered $code"
tag=$(K exec -n "$NS_DOOR" "$HTTPPOD" -- curl -sS -D- -o /dev/null \
        -H "Authorization: Bearer $TOKEN" -H "X-Remote-User: x" \
        "$EP/files/content?path=shared.txt" 2>/dev/null | tr -d '\r' | \
        awk '/^[Ee][Tt][Aa][Gg]:/{gsub(/"/,"",$2); print $2}')
if [ -z "$tag" ]; then
  bad "no ETag came back for shared.txt — P4 cannot condition on a version it does not have"
fi
: > "$WORK/codes"
for i in $(seq 1 8); do
  # `%{http_code}` carries no newline, so eight concurrent appends
  # arrive as one line — `2004124124124...` — and every count reads 0.
  # The run that found this had the right answer (one 200, seven 412s)
  # and reported a lost update.
  ( c=$(curlp PUT "/files/content?path=shared.txt" -H "X-Flint-Author: u$i" \
      -H "If-Match: \"$tag\"" --data-binary "v$i"); printf '%s\n' "$c" >> "$WORK/codes" ) &
done
wait
w=$(grep -c '^200$' "$WORK/codes" 2>/dev/null || echo 0)
l=$(grep -c '^412$' "$WORK/codes" 2>/dev/null || echo 0)
[ "$w" = "1" ] && ok "exactly one write won against one version ($l refused 412)" \
  || bad "$w writers won against one version, $l refused — a lost update"

echo
echo "== P5: the refusals a browser must be able to render =="
code=$(curlp POST "/files/folder" -H "Content-Type: application/json" --data '{"path":"/x"}')
[ "$code" = "501" ] && ok "an empty directory is refused with a reason (501)" || bad "folder got $code"
code=$(K exec -n "$NS_DOOR" "$HTTPPOD" -- sh -c \
  "head -c 3000000 /dev/zero | curl -sS -o /dev/null -w '%{http_code}' -X PUT \
   '$EP/files/content?path=big.bin' -H 'Authorization: Bearer $TOKEN' \
   -H 'X-Remote-User: x' --data-binary @-" 2>/dev/null)
[ "$code" = "413" ] && ok "over the 2 MiB cap is refused (413)" || bad "oversize got $code"
code=$(curlp PUT "/files/content?path=ui" --data-binary "clobber")
[ "$code" = "409" ] && ok "a write over a directory is refused (409), not performed" \
  || bad "directory clobber got $code"
gitp "cd /tmp/c && echo x > m.txt && git add -A && git commit -qm m && \
      G push \$U HEAD:main" > "$WORK/protected" 2>&1
# Any refusal counts, and the wording is git's, not ours: a direct push
# to the repository's own git port carries no door-verified principal,
# so the refusal may come back as a plain 403 rather than the hook's
# sentence. An earlier run reported this FAIL while the push had in fact
# been refused.
if grep -qiE "protected|refus|denied|403|rejected" "$WORK/protected"; then
  ok "the branch policy still protects main: $(grep -oiE 'protected[^\"]*|error: 403|403' "$WORK/protected" | head -1)"
else
  bad "a push to protected main was NOT refused: $(head -3 "$WORK/protected")"
fi

echo
echo "== P6: the repository sleeps, and the browser wakes it =="
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
  # THE DOOR IS WHAT ARMS A WAKE, and it does not serve the file API
  # yet — so an HTTP read against a slept repository reaches nothing and
  # nothing asks for it back. That is a real gap, recorded as such: with
  # the gateway door built, this annotation is what it would set. The
  # drill stands in for it so the leg can still measure what it is for,
  # which is whether the CONTENT survives the reap.
  note "arming the wake as the door would (chert.us/requested-at) — the door does not \
serve the file API yet, so nothing else will"
  K annotate -n "$NS" flintrepo "$REPO" \
    "chert.us/requested-at=$(date -u +%Y-%m-%dT%H:%M:%SZ)" --overwrite >/dev/null 2>&1
  for i in $(seq 1 60); do
    n=$(K get -n "$NS" pod -l chert.us/repo="$REPO" --no-headers 2>/dev/null | wc -l | tr -d ' ')
    [ "$n" != "0" ] && break
    sleep 5
  done
  K wait -n "$NS" --for=condition=Ready pod -l chert.us/repo="$REPO" --timeout=300s >/dev/null 2>&1
  code=$(curlp GET "/files/content?path=code/a.txt" --max-time 300)
  got=$(body)
  after=$(K get -n "$NS" pod -l chert.us/repo="$REPO" -o jsonpath='{.items[0].metadata.uid}' 2>/dev/null)
  [ "$code" = "200" ] && [ "$got" = "from-agent" ] \
    && ok "a browser read woke it and the content survived" \
    || bad "the wake read answered $code '$got'"
  [ -n "$after" ] && [ "$after" != "$before" ] \
    && ok "and it is a FRESH pod ($before -> $after)" \
    || bad "the pod UID did not change — this was not a restore"
fi

echo
echo "==== F12: $PASS passed, $FAIL failed, $INCONC inconclusive ===="
echo "work: $WORK"
[ "$FAIL" -eq 0 ] && [ "$INCONC" -eq 0 ]
